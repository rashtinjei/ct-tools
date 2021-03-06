use ct_tools::common::{sha256_hex, Log};
use ct_tools::ct::submit_cert_to_logs;
use ct_tools::google::{fetch_all_ct_logs, fetch_trusted_ct_logs};
use ct_tools::{crtsh, letsencrypt};
use futures::stream::{StreamExt, TryStreamExt};
use net2::unix::UnixTcpBuilderExt;
use rustls::Session;
use std::fs::{self, File};
use std::io::Read;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use structopt::StructOpt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

fn pems_to_chain(data: &[u8]) -> Vec<Vec<u8>> {
    pem::parse_many(data)
        .into_iter()
        .filter(|p| p.tag == "CERTIFICATE")
        .map(|p| p.contents)
        .collect()
}

fn compute_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .flat_map(|p| {
            if fs::metadata(p).unwrap().is_dir() {
                fs::read_dir(p)
                    .unwrap()
                    .map(|d| d.unwrap().path().to_str().unwrap().to_string())
                    .collect()
            } else {
                vec![p.clone()]
            }
        })
        .collect()
}

async fn submit(paths: &[String], all_logs: bool) {
    let http_client = reqwest::Client::new();

    let logs = if all_logs {
        fetch_all_ct_logs(&http_client).await
    } else {
        fetch_trusted_ct_logs(&http_client).await
    };

    let all_paths = compute_paths(paths);

    let work = all_paths
        .iter()
        .map(|path| {
            let path = path.to_string();
            let mut contents = Vec::new();
            File::open(&path)
                .unwrap()
                .read_to_end(&mut contents)
                .unwrap();

            let http_client = &http_client;
            let logs = &logs;
            let mut chain = pems_to_chain(&contents);
            async move {
                if chain.len() == 1 {
                    // TODO: There's got to be some way to do this ourselves, instead of using crt.sh
                    // as a glorified AIA chaser.
                    println!(
                        "[{}] Only one certificate in chain, using crt.sh to build a full chain ...",
                        &path
                    );
                    let new_chain = crtsh::build_chain_for_cert(&http_client, &chain[0]).await;
                    chain = match new_chain {
                        Ok(c) => c,
                        Err(()) => {
                            println!("[{}] Unable to build a chain", path);
                            return futures::future::ready(());
                        }
                    }
                }
                println!("[{}] Submitting ...", &path);
                let timeout = Duration::from_secs(30);
                let scts = submit_cert_to_logs(&http_client, &logs, &chain, timeout).await;

                if !scts.is_empty() {
                    println!(
                        "[{}] Find the cert on crt.sh: {}",
                        path,
                        crtsh::url_for_cert(&chain[0])
                    );
                    let mut table = prettytable::Table::new();
                    table.add_row(prettytable::Row::new(vec![prettytable::Cell::new("Log")]));
                    for (log_idx, _) in scts {
                        let log = &logs[log_idx];
                        table.add_row(prettytable::Row::new(vec![prettytable::Cell::new(
                            &log.description,
                        )]));
                    }
                    table.printstd();
                    println!();
                    println!();
                } else {
                    println!("[{}] No SCTs obtained", &path);
                }

                futures::future::ready(())
            }
        })
        .collect::<futures::stream::FuturesOrdered<_>>()
        .buffered(4)
        .for_each(move |()| async {});
    work.await;
}

async fn check(paths: &[String]) {
    let http_client = reqwest::Client::new();

    let all_paths = compute_paths(paths);

    let work = all_paths
        .iter()
        .map(|path| {
            let path = path.to_string();
            let mut contents = Vec::new();
            File::open(&path)
                .unwrap()
                .read_to_end(&mut contents)
                .unwrap();

            let http_client = &http_client;
            let chain = pems_to_chain(&contents);
            async move {
                if !chain.is_empty() && crtsh::is_cert_logged(&http_client, &chain[0]).await {
                    println!("{} was already logged", path);
                } else {
                    println!("{} has not been logged", path);
                }

                futures::future::ready(())
            }
        })
        .collect::<futures::stream::FuturesOrdered<_>>()
        .buffered(16)
        .for_each(move |()| async {});
    work.await;
}

async fn handle_request(
    request: hyper::Request<hyper::Body>,
    templates: Arc<tera::Tera>,
    http_client: Arc<reqwest::Client>,
    logs: Arc<Vec<Log>>,
    client_cert: Option<rustls::Certificate>,
) -> Result<hyper::Response<hyper::Body>, hyper::Error> {
    let mut crtsh_url = None;
    let mut rendered_cert = None;
    if let Some(cert) = client_cert {
        if request.method() == hyper::Method::POST {
            if let Ok(chain) = crtsh::build_chain_for_cert(&http_client, &cert.0).await {
                let timeout = Duration::from_secs(5);
                let scts = submit_cert_to_logs(&http_client, &logs, &chain, timeout).await;
                if !scts.is_empty() {
                    crtsh_url = Some(crtsh::url_for_cert(&chain[0]));
                    println!("Successfully submitted: {}", sha256_hex(&chain[0]));
                }
            }
        }

        let mut process = Command::new("openssl")
            .arg("x509")
            .arg("-text")
            .arg("-noout")
            .arg("-inform")
            .arg("der")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let cert_bytes = cert.0.clone();
        AsyncWriteExt::write_all(&mut process.stdin.as_mut().unwrap(), &cert_bytes)
            .await
            .unwrap();
        let out = process.wait_with_output().await.unwrap();
        rendered_cert = Some(String::from_utf8_lossy(&out.stdout).into_owned());
    }

    let mut context = tera::Context::new();
    context.insert("cert", &rendered_cert);
    context.insert("crtsh_url", &crtsh_url);
    let body = templates.render("home.html", &context).unwrap();
    Ok(hyper::Response::builder()
        .status(hyper::StatusCode::OK)
        .body(body.into())
        .unwrap())
}

struct NoVerificationCertificateVerifier;
impl rustls::ClientCertVerifier for NoVerificationCertificateVerifier {
    fn client_auth_mandatory(&self, _: Option<&webpki::DNSName>) -> Option<bool> {
        Some(false)
    }

    fn verify_client_cert(
        &self,
        _: &[rustls::Certificate],
        _: Option<&webpki::DNSName>,
    ) -> Result<rustls::ClientCertVerified, rustls::TLSError> {
        Ok(rustls::ClientCertVerified::assertion())
    }

    fn client_auth_root_subjects(
        &self,
        _: Option<&webpki::DNSName>,
    ) -> Option<rustls::DistinguishedNames> {
        Some(rustls::DistinguishedNames::new())
    }
}

async fn server(local_dev: bool, domain: Option<&str>, letsencrypt_env: Option<&str>) {
    match (local_dev, domain, letsencrypt_env) {
        (true, Some(_), _) | (true, _, Some(_)) => {
            panic!("Can't use both --local-dev and --letsencrypt-env or --domain")
        }
        (_, Some(_), None) | (_, None, Some(_)) => {
            panic!("When using Let's Encrypt, must set both --letsencrypt-env and --domain")
        }
        (false, _, None) => panic!("Must set at least one of --local-dev or --letsencrypt-env"),
        _ => {}
    };

    // Disable certificate verification. In any normal context, this would be horribly insecure!
    // However, all we're doing is taking the certs and then turning around and submitting them to
    // CT logs, so it doesn't matter if they're verified.
    let mut tls_config = rustls::ServerConfig::new(Arc::new(NoVerificationCertificateVerifier));
    if local_dev {
        // TODO: not all the details on the cert are perfect, but it's fine.
        let (cert, pkey) = letsencrypt::generate_temporary_cert("localhost");
        tls_config
            .set_single_cert(
                vec![letsencrypt::openssl_cert_to_rustls(&cert)],
                letsencrypt::openssl_pkey_to_rustls(&pkey),
            )
            .unwrap();
    } else {
        let letsencrypt_url = match letsencrypt_env.unwrap() {
            "prod" => "https://acme-v01.api.letsencrypt.org/directory",
            "dev" => "https://acme-staging.api.letsencrypt.org/directory",
            _ => unreachable!(),
        };
        let cert_cache = letsencrypt::DiskCache::new(
            dirs::home_dir()
                .unwrap()
                .join(".ct-tools")
                .join("certificates"),
        );
        tls_config.cert_resolver = Arc::new(letsencrypt::AutomaticCertResolver::new(
            letsencrypt_url,
            vec![domain.unwrap().to_string()],
            cert_cache,
        ));
    }
    tls_config.set_persistence(rustls::ServerSessionMemoryCache::new(1024));
    tls_config.ticketer = rustls::Ticketer::new();

    let http_client = Arc::new(reqwest::Client::new());
    let logs = Arc::new(fetch_trusted_ct_logs(&http_client).await);
    let templates = Arc::new(tera::Tera::new("templates/*").unwrap());

    let addr = if local_dev {
        "0.0.0.0:8000"
    } else {
        "0.0.0.0:443"
    }
    .parse()
    .unwrap();

    // If there aren't at least two threads, the Let's Encrypt integration will deadlock.
    println!("Listening on https://{} ...", addr);
    let std_listener = match addr {
        SocketAddr::V4(_) => net2::TcpBuilder::new_v4().unwrap(),
        SocketAddr::V6(_) => net2::TcpBuilder::new_v6().unwrap(),
    };
    std_listener.reuse_port(true).unwrap();
    std_listener.reuse_address(true).unwrap();
    std_listener.bind(addr).unwrap();
    let tls_acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
    let mut listener =
        tokio::net::TcpListener::from_std(std_listener.listen(1024).unwrap()).unwrap();
    let connections = listener
        .incoming()
        .and_then(move |sock| tls_acceptor.accept(sock))
        .filter(move |s| futures::future::ready(s.is_ok()))
        .boxed();
    let server = hyper::Server::builder(hyper::server::accept::from_stream(connections)).serve(
        hyper::service::make_service_fn(
            move |conn: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>| {
                let http_client = Arc::clone(&http_client);
                let templates = Arc::clone(&templates);
                let logs = Arc::clone(&logs);
                let client_cert = conn
                    .get_ref()
                    .1
                    .get_peer_certificates()
                    .map(|mut chain| chain.remove(0));

                async {
                    Ok::<_, hyper::Error>(hyper::service::service_fn(
                        move |r: hyper::Request<hyper::Body>| {
                            handle_request(
                                r,
                                Arc::clone(&templates),
                                Arc::clone(&http_client),
                                Arc::clone(&logs),
                                client_cert.clone(),
                            )
                        },
                    ))
                }
            },
        ),
    );

    server.await.unwrap()
}

#[derive(StructOpt)]
#[structopt(name = "ct-tools")]
enum Opt {
    #[structopt(name = "submit", about = "Directly submits certificates to CT logs")]
    Submit {
        #[structopt(
            long = "all-logs",
            help = "Submit to all logs, instead of just ones trusted by Chrome"
        )]
        all_logs: bool,
        #[structopt(help = "Path to certificate or chain")]
        paths: Vec<String>,
    },

    #[structopt(
        name = "check",
        about = "Checks whether a certificate exists in CT logs"
    )]
    Check {
        #[structopt(help = "Path to certificate or chain")]
        paths: Vec<String>,
    },

    #[structopt(
        name = "server",
        about = "Run an HTTPS server that submits client certificates to CT logs"
    )]
    Server {
        #[structopt(
            long = "--local-dev",
            help = "Local development, do not obtain a certificate"
        )]
        local_dev: bool,
        #[structopt(long = "domain", help = "Domain this is running as")]
        domain: Option<String>,
        #[structopt(
            long = "letsencrypt-env",
            possible_value = "dev",
            possible_value = "prod",
            help = "Let's Encrypt environment to use"
        )]
        letsencrypt_env: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    match Opt::from_args() {
        Opt::Submit { paths, all_logs } => {
            submit(&paths, all_logs).await;
        }
        Opt::Check { paths } => {
            check(&paths).await;
        }
        Opt::Server {
            local_dev,
            domain,
            letsencrypt_env,
        } => {
            server(
                local_dev,
                domain.as_ref().map(|s| s.as_ref()),
                letsencrypt_env.as_ref().map(|s| s.as_ref()),
            )
            .await;
        }
    }
}
