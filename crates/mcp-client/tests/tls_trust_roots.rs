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

use std::sync::Arc;

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

#[tokio::test]
async fn the_api_client_trusts_an_authority_named_by_ssl_cert_file() {
    // This integration-test process contains only this test, so its
    // environment cannot race another test's.
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    for proxy in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        std::env::remove_var(proxy);
    }
    std::env::remove_var("SSL_CERT_DIR");
    std::env::remove_var("SSL_CERT_FILE");
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

    // The machine names it, as a TLS-inspecting proxy's setup does.
    let authority_file = home.path().join("proxy-ca.pem");
    std::fs::write(&authority_file, authority_pem).unwrap();
    std::env::set_var("SSL_CERT_FILE", &authority_file);
    let user = api_client(&url)
        .me()
        .await
        .expect("the authority in SSL_CERT_FILE is trusted");
    assert_eq!(user.email, "fixture@example.test");

    // A machine with no usable store (a bare container, a path that is not
    // there) still gets a client, which has the bundled roots and nothing
    // else: it does not trust the throwaway authority.
    std::env::set_var("SSL_CERT_FILE", home.path().join("not-there.pem"));
    let refused = api_client(&url)
        .get_once::<serde_json::Value>("/auth/me")
        .await
        .expect_err("the bundled roots do not hold the throwaway authority");
    assert!(
        format!("{refused:?}").contains("certificate"),
        "{refused:?}"
    );
}

/// Both sets of roots are compiled in: the bundled ones, for a machine with
/// no store, and the machine's own. Every HTTP client in the workspace takes
/// its features from this one line.
#[test]
fn reqwest_is_built_with_the_bundled_roots_and_the_machines_own() {
    let manifest = include_str!("../../../Cargo.toml");
    let reqwest = manifest
        .lines()
        .find(|line| line.starts_with("reqwest = "))
        .expect("the workspace's reqwest dependency");
    for feature in ["\"rustls-tls\"", "\"rustls-tls-native-roots\""] {
        assert!(reqwest.contains(feature), "{feature} missing in: {reqwest}");
    }
}
