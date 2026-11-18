//! The API client trusts the certificate authorities the machine trusts, not
//! only the ones bundled into the binary.
//!
//! Behind a TLS-inspecting proxy (a corporate laptop, a cloud agent VM) every
//! answer is signed by the proxy's own authority. The machine names it in its
//! certificate store or in `SSL_CERT_FILE`; a client that trusts only its
//! bundled roots fails every API call there with an untrusted certificate.
//!
//! A local TLS server stands in for the proxy. Its authority is made for the
//! run: no certificate or key is read from the repository, and none is kept.
//!
//! The machine's store is never a reason a client cannot be made. A hook is a
//! fresh process on every prompt and makes a client each time, so a store
//! that cannot be read, or that holds nothing usable, leaves the bundled
//! roots and is said once, on the log.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use mcp_client::ContextStreamClient;
use mcp_types::config::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::crypto::ring::{default_provider, sign::any_eddsa_type};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::sign::SigningKey;
use tokio_rustls::rustls::{ServerConfig, SignatureScheme};
use tokio_rustls::TlsAcceptor;

/// One DER element: tag, length, content.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match content.len() {
        len if len < 0x80 => out.push(len as u8),
        len if len <= 0xff => out.extend([0x81, len as u8]),
        len => out.extend([0x82, (len >> 8) as u8, len as u8]),
    }
    out.extend_from_slice(content);
    out
}

fn sequence(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}

/// A new Ed25519 key: PKCS#8 around 32 bytes from the OS.
fn new_key() -> (PrivatePkcs8KeyDer<'static>, Arc<dyn SigningKey>) {
    let mut pkcs8 = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    pkcs8.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    pkcs8.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let pkcs8 = PrivatePkcs8KeyDer::from(pkcs8);
    let key = any_eddsa_type(&pkcs8).expect("an Ed25519 key");
    (pkcs8, key)
}

/// `CN=<common_name>`.
fn name(common_name: &str) -> Vec<u8> {
    let attribute = sequence(&[
        der(0x06, &[0x55, 0x04, 0x03]),
        der(0x0c, common_name.as_bytes()),
    ]);
    sequence(&[der(0x31, &attribute)])
}

/// An X.509 v3 certificate for `subject`'s key, signed by `issuer`'s, valid
/// from 2020 to 2100, with the given extensions.
fn certificate(
    subject: (&str, &dyn SigningKey),
    issuer: (&str, &dyn SigningKey),
    extensions: &[Vec<u8>],
) -> CertificateDer<'static> {
    let ed25519 = sequence(&[der(0x06, &[0x2b, 0x65, 0x70])]);
    let validity = sequence(&[der(0x17, b"200101000000Z"), der(0x18, b"21000101000000Z")]);
    let public_key = subject.1.public_key().expect("the key's public half");
    let to_be_signed = sequence(&[
        der(0xa0, &der(0x02, &[2])),
        der(0x02, &[1]),
        ed25519.clone(),
        name(issuer.0),
        validity,
        name(subject.0),
        public_key.as_ref().to_vec(),
        der(0xa3, &sequence(extensions)),
    ]);
    let signature = issuer
        .1
        .choose_scheme(&[SignatureScheme::ED25519])
        .expect("an Ed25519 signer")
        .sign(&to_be_signed)
        .expect("a signature");
    let signature = der(0x03, &[&[0u8][..], &signature].concat());
    CertificateDer::from(sequence(&[to_be_signed, ed25519, signature]))
}

/// One extension: its OID (under 2.5.29), whether it is critical, its value.
fn extension(oid: u8, critical: bool, value: &[u8]) -> Vec<u8> {
    let mut parts = vec![der(0x06, &[0x55, 0x1d, oid])];
    if critical {
        parts.push(der(0x01, &[0xff]));
    }
    parts.push(der(0x04, value));
    sequence(&parts)
}

/// A throwaway authority and, signed by it, a TLS server on 127.0.0.1 that
/// answers every request as the API's `/auth/me` does. Returns the
/// authority's certificate as PEM and the server's URL.
async fn serve_behind_a_throwaway_authority() -> (String, String) {
    let (_, authority_key) = new_key();
    let basic_constraints_ca = sequence(&[der(0x01, &[0xff])]);
    let authority = certificate(
        ("Throwaway test authority", authority_key.as_ref()),
        ("Throwaway test authority", authority_key.as_ref()),
        &[extension(19, true, &basic_constraints_ca)],
    );

    let (server_pkcs8, server_key) = new_key();
    let subject_alt_name = sequence(&[der(0x87, &[127, 0, 0, 1])]);
    let server = certificate(
        ("127.0.0.1", server_key.as_ref()),
        ("Throwaway test authority", authority_key.as_ref()),
        &[extension(17, false, &subject_alt_name)],
    );

    let config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![server], PrivateKeyDer::Pkcs8(server_pkcs8))
        .expect("the server's certificate and key");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let url = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    );

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                // A client that does not trust the authority ends the handshake here.
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut request = Vec::new();
                let mut chunk = [0u8; 2048];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match tls.read(&mut chunk).await {
                        Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
                        _ => return,
                    }
                }
                let body = r#"{"id":"00000000-0000-4000-8000-000000000042","email":"fixture@example.test","created_at":"2026-01-01T00:00:00Z"}"#;
                let answer = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(answer.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });

    let pem = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        base64::engine::general_purpose::STANDARD.encode(authority.as_ref())
    );
    (pem, url)
}

/// The client as the product makes it (`ContextStreamClient::new`).
fn api_client(url: &str) -> ContextStreamClient {
    ContextStreamClient::new(Config {
        api_url: url.to_string(),
        api_key: Some("test-key".to_string()),
        ..Config::default()
    })
}

/// The tests below set `SSL_CERT_FILE` and `SSL_CERT_DIR` for the whole
/// process, which holds nothing but this file: one of them at a time.
static ENVIRONMENT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A home of its own and no proxy or certificate variables.
fn clean_environment() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "SSL_CERT_DIR",
        "SSL_CERT_FILE",
    ] {
        std::env::remove_var(name);
    }
    home
}

fn set_or_remove(name: &str, value: Option<&Path>) {
    match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

/// What this thread logs at WARN or above, while the guard lives.
#[derive(Clone, Default)]
struct Logged(Arc<Mutex<Vec<u8>>>);

impl Logged {
    fn start() -> (Self, tracing::subscriber::DefaultGuard) {
        let logged = Self::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logged.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        (logged, tracing::subscriber::set_default(subscriber))
    }

    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl std::io::Write for Logged {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for Logged {
    type Writer = Logged;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn the_api_client_trusts_an_authority_named_by_ssl_cert_file() {
    let _one_at_a_time = ENVIRONMENT.lock().await;
    let home = clean_environment();
    let (authority_pem, url) = serve_behind_a_throwaway_authority().await;

    // Nobody named the authority: the answer is refused, and the error says
    // it is about the certificate.
    let refused = api_client(&url)
        .get_once::<serde_json::Value>("/auth/me")
        .await
        .expect_err("an authority nobody named is not trusted");
    let refused = format!("{refused:?}");
    assert!(
        refused.contains("certificate") || refused.contains("UnknownIssuer"),
        "{refused}"
    );

    // The machine names it, as a TLS-inspecting proxy's setup does: in a
    // file, or in a directory of certificates.
    let authority_file = home.path().join("proxy-ca.pem");
    std::fs::write(&authority_file, &authority_pem).unwrap();
    let authority_dir = home.path().join("certs");
    std::fs::create_dir(&authority_dir).unwrap();
    std::fs::write(authority_dir.join("proxy-ca.pem"), &authority_pem).unwrap();
    for (file, dir) in [
        (Some(authority_file.as_path()), None),
        (None, Some(authority_dir.as_path())),
    ] {
        set_or_remove("SSL_CERT_FILE", file);
        set_or_remove("SSL_CERT_DIR", dir);
        let (logged, _guard) = Logged::start();
        let user = api_client(&url)
            .me()
            .await
            .expect("the authority the machine names is trusted");
        assert_eq!(user.email, "fixture@example.test");
        assert_eq!(logged.lines(), Vec::<String>::new());
    }
}

// 1.0.82 ignored SSL_CERT_FILE. Reading it must not turn a variable that
// names nothing usable into a client that cannot be made: that was a panic
// ("zero valid certificates found in native root store") in every hook and
// every call on that machine.
#[tokio::test]
async fn a_store_that_cannot_be_used_leaves_the_bundled_roots_and_is_said_once() {
    let _one_at_a_time = ENVIRONMENT.lock().await;
    let home = clean_environment();
    let (_, url) = serve_behind_a_throwaway_authority().await;

    let at = |name: &str| home.path().join(name);
    // A certificate's wrapping around bytes that are not one.
    let not_a_certificate =
        "-----BEGIN CERTIFICATE-----\nbm90IGEgY2VydGlmaWNhdGUsIGp1c3Qgd29yZHM=\n-----END CERTIFICATE-----\n";
    std::fs::write(at("not-a-certificate.pem"), not_a_certificate).unwrap();
    std::fs::write(at("noise.pem"), [0u8, 159, 146, 150, 10, 255, 254]).unwrap();
    std::fs::create_dir(at("a-directory")).unwrap();
    std::fs::create_dir(at("bad-certificates")).unwrap();
    std::fs::write(at("bad-certificates/one.pem"), not_a_certificate).unwrap();
    std::fs::write(at("unreadable.pem"), not_a_certificate).unwrap();
    std::fs::create_dir(at("unreadable-directory")).unwrap();
    std::fs::write(at("unreadable-directory/one.pem"), not_a_certificate).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["unreadable.pem", "unreadable-directory"] {
            std::fs::set_permissions(at(name), std::fs::Permissions::from_mode(0o000)).unwrap();
        }
    }

    let cases: [(&str, Option<PathBuf>, Option<PathBuf>); 9] = [
        (
            "a file that holds no real certificate",
            Some(at("not-a-certificate.pem")),
            None,
        ),
        ("a file that is not PEM", Some(at("noise.pem")), None),
        (
            "a directory where a file is expected",
            Some(at("a-directory")),
            None,
        ),
        (
            "a file that cannot be read",
            Some(at("unreadable.pem")),
            None,
        ),
        ("a file that is not there", Some(at("not-there.pem")), None),
        (
            "a directory of bad certificates",
            None,
            Some(at("bad-certificates")),
        ),
        (
            "a file where a directory is expected",
            None,
            Some(at("not-a-certificate.pem")),
        ),
        (
            "a directory that cannot be read",
            None,
            Some(at("unreadable-directory")),
        ),
        ("a directory that is not there", None, Some(at("not-there"))),
    ];
    for (case, file, dir) in cases {
        set_or_remove("SSL_CERT_FILE", file.as_deref());
        set_or_remove("SSL_CERT_DIR", dir.as_deref());
        let (logged, _guard) = Logged::start();

        // Three clients, as a process that makes more than one does: none of
        // them fails to be made, and each still has the bundled roots only
        // (it reaches the server and refuses its throwaway authority).
        for _ in 0..3 {
            let refused = api_client(&url)
                .get_once::<serde_json::Value>("/auth/me")
                .await
                .expect_err("the bundled roots do not hold the throwaway authority");
            assert!(
                format!("{refused:?}").contains("certificate"),
                "{case}: {refused:?}"
            );
        }

        // Said once, in plain words: which variable, and what is trusted now.
        let lines = logged.lines();
        assert_eq!(lines.len(), 1, "{case}: {lines:?}");
        let (variable, named) = match (&file, &dir) {
            (Some(file), _) => ("SSL_CERT_FILE", file),
            (_, Some(dir)) => ("SSL_CERT_DIR", dir),
            _ => unreachable!(),
        };
        assert!(lines[0].contains("WARN"), "{case}: {lines:?}");
        assert!(lines[0].contains(variable), "{case}: {lines:?}");
        assert!(
            lines[0].contains(&*named.to_string_lossy()),
            "{case}: {lines:?}"
        );
        assert!(lines[0].contains("bundled"), "{case}: {lines:?}");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["unreadable.pem", "unreadable-directory"] {
            std::fs::set_permissions(at(name), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}

/// reqwest is built with the bundled roots only. It does not read the
/// machine's store itself ("rustls-tls-native-roots"): with that, a store
/// whose certificates all fail to parse fails the build of the client.
/// `mcp_client::http` reads the store, and keeps what can be used.
#[test]
fn reqwest_has_the_bundled_roots_and_does_not_read_the_machines_store_itself() {
    let manifest = include_str!("../../../Cargo.toml");
    let reqwest = manifest
        .lines()
        .find(|line| line.starts_with("reqwest = "))
        .expect("the workspace's reqwest dependency");
    assert!(reqwest.contains("\"rustls-tls\""), "{reqwest}");
    assert!(!reqwest.contains("native-roots"), "{reqwest}");
    assert!(!include_str!("../Cargo.toml").contains("rustls-native-certs"));
}

/// One code path: a client made anywhere else would not trust what the
/// machine trusts. Every `.rs` file of every crate is read, tests included.
#[test]
fn every_http_client_in_the_workspace_is_made_by_mcp_client_http() {
    fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }

    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let the_one_place = crates.join("mcp-client").join("src").join("http.rs");
    let this_file = crates
        .join("mcp-client")
        .join("tests")
        .join("tls_trust_roots.rs");
    let mut files = Vec::new();
    for member in std::fs::read_dir(crates).unwrap() {
        for part in ["src", "tests", "benches", "examples"] {
            let dir = member.as_ref().unwrap().path().join(part);
            if dir.is_dir() {
                rust_files(&dir, &mut files);
            }
        }
    }
    assert!(files.len() > 100, "{} files", files.len());
    assert!(the_one_place.is_file(), "no {}", the_one_place.display());

    // `reqwest::Client::builder()`, `reqwest::Client::new()`, the same after
    // `use reqwest::Client`, and `reqwest::ClientBuilder`.
    let makes_a_client =
        regex::Regex::new(r"(^|[^A-Za-z0-9_])Client::(builder|new)\(|reqwest::ClientBuilder")
            .unwrap();
    let mut elsewhere = Vec::new();
    for file in files {
        if file == the_one_place || file == this_file {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for (number, line) in text.lines().enumerate() {
            if makes_a_client.is_match(line) {
                elsewhere.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(crates).unwrap().display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        elsewhere.is_empty(),
        "made outside mcp_client::http:\n{}",
        elsewhere.join("\n")
    );
}
