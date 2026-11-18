//! Where every HTTP client of the workspace is made.
//!
//! A client trusts two sets of certificate authorities:
//!
//! - the ones bundled into the binary (reqwest's "rustls-tls"), so a
//!   container with no certificate store still connects;
//! - the ones this machine trusts (its certificate store, or `SSL_CERT_FILE`
//!   and `SSL_CERT_DIR` when they are set), so the authority of a
//!   TLS-inspecting proxy is trusted where the machine trusts it.
//!
//! The machine's store is read here and not by reqwest
//! ("rustls-tls-native-roots"). reqwest refuses to build a client when the
//! store holds certificates and none of them parses, and a client that
//! cannot be built is a panic in most callers: one bad `SSL_CERT_FILE` then
//! breaks every hook and every call on the machine. Here the store can only
//! add to what is trusted. One that cannot be read, or that holds nothing
//! usable, is said once on the log, and the client has the bundled roots.
//!
//! The store is read once per process, by the first client that is made:
//! on macOS that is a question to the keychain, on Linux a few hundred
//! files. `tests/tls_trust_roots.rs` keeps every other file of the
//! workspace from making a client by itself.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// `SSL_CERT_FILE` and `SSL_CERT_DIR` as they were when the store was read.
type Named = (Option<OsString>, Option<OsString>);

/// The machine's certificate authorities, and what named them.
struct MachineRoots {
    named: Named,
    roots: Arc<[reqwest::Certificate]>,
}

static MACHINE_ROOTS: Mutex<Option<MachineRoots>> = Mutex::new(None);

/// A `reqwest::ClientBuilder` that trusts what this machine trusts beside the
/// bundled roots. Use it wherever `reqwest::Client::builder()` would be.
pub fn client_builder() -> reqwest::ClientBuilder {
    machine_roots()
        .iter()
        .cloned()
        .fold(reqwest::Client::builder(), |builder, root| {
            builder.add_root_certificate(root)
        })
}

/// A client with reqwest's default settings. Use it wherever
/// `reqwest::Client::new()` would be; like it, this panics only when no TLS
/// backend can be started at all.
pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .or_else(|_| reqwest::Client::builder().build())
        .expect("an HTTP client with default settings")
}

/// The machine's certificate authorities, read the first time and again only
/// when `SSL_CERT_FILE` or `SSL_CERT_DIR` has changed since.
fn machine_roots() -> Arc<[reqwest::Certificate]> {
    let named = (
        std::env::var_os("SSL_CERT_FILE"),
        std::env::var_os("SSL_CERT_DIR"),
    );
    let mut held = MACHINE_ROOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(held) = held.as_ref().filter(|held| held.named == named) {
        return held.roots.clone();
    }

    let started = Instant::now();
    let found = rustls_native_certs::load_native_certs();
    let errors: Vec<String> = found.errors.iter().map(ToString::to_string).collect();
    let (roots, problem) = usable_roots(
        found.certs.iter().map(|certificate| certificate.as_ref()),
        &errors,
        &named,
    );
    if let Some(problem) = problem {
        tracing::warn!("{problem}");
    }
    tracing::debug!(
        "Trusting {} certificate authorities of this machine beside the bundled ones (read in {:?})",
        roots.len(),
        started.elapsed()
    );
    let roots: Arc<[reqwest::Certificate]> = roots.into();
    *held = Some(MachineRoots {
        named,
        roots: roots.clone(),
    });
    roots
}

/// The certificates a client can take as roots, and what to say when there
/// is none where some were expected.
///
/// reqwest adds a root with `rustls::RootCertStore::add` and fails the whole
/// client when one is refused, so each certificate is tried with the same
/// call first and left out when it fails.
fn usable_roots<'a>(
    certificates: impl Iterator<Item = &'a [u8]>,
    errors: &[String],
    named: &Named,
) -> (Vec<reqwest::Certificate>, Option<String>) {
    let mut tried = rustls::RootCertStore::empty();
    let mut roots = Vec::new();
    let mut refused = 0usize;
    for der in certificates {
        let usable = tried
            .add(rustls::pki_types::CertificateDer::from(der))
            .is_ok();
        match reqwest::Certificate::from_der(der) {
            Ok(root) if usable => roots.push(root),
            _ => refused += 1,
        }
    }
    if !roots.is_empty() {
        return (roots, None);
    }

    let shown = |value: &OsString| value.to_string_lossy().into_owned();
    let source = match named {
        (Some(file), Some(dir)) => format!(
            "SSL_CERT_FILE ({}) and SSL_CERT_DIR ({})",
            shown(file),
            shown(dir)
        ),
        (Some(file), None) => format!("SSL_CERT_FILE ({})", shown(file)),
        (None, Some(dir)) => format!("SSL_CERT_DIR ({})", shown(dir)),
        // A machine with no store at all, a bare container: nothing was
        // expected, and the bundled roots are what it has always had.
        (None, None) if errors.is_empty() && refused == 0 => return (roots, None),
        (None, None) => "This machine's certificate store".to_string(),
    };
    let why = match errors.first() {
        Some(error) => error.clone(),
        None if refused > 0 => format!("none of the {refused} found there could be read"),
        None => "none was found there".to_string(),
    };
    let problem = format!(
        "{source} gave no certificate authority that can be used ({why}). Only the certificate \
         authorities bundled into this program are trusted: calls that pass through an \
         HTTPS-inspecting proxy will fail until that is put right."
    );
    (roots, Some(problem))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(file: Option<&str>, dir: Option<&str>) -> Named {
        (file.map(OsString::from), dir.map(OsString::from))
    }

    #[test]
    fn a_machine_with_no_store_is_not_a_problem() {
        let (roots, problem) = usable_roots(std::iter::empty(), &[], &named(None, None));
        assert!(roots.is_empty());
        assert_eq!(problem, None);
    }

    #[test]
    fn a_store_that_gives_nothing_usable_says_what_it_was_and_why() {
        let not_a_certificate: &[u8] = b"not a certificate";
        let cases = [
            (
                named(Some("/etc/proxy.pem"), None),
                vec![not_a_certificate],
                vec![],
                "SSL_CERT_FILE (/etc/proxy.pem) gave no certificate authority that can be used \
                 (none of the 1 found there could be read).",
            ),
            (
                named(None, Some("/etc/certs")),
                vec![],
                vec!["could not read /etc/certs".to_string()],
                "SSL_CERT_DIR (/etc/certs) gave no certificate authority that can be used \
                 (could not read /etc/certs).",
            ),
            (
                named(Some("/a.pem"), Some("/certs")),
                vec![],
                vec![],
                "SSL_CERT_FILE (/a.pem) and SSL_CERT_DIR (/certs) gave no certificate authority \
                 that can be used (none was found there).",
            ),
            (
                named(None, None),
                vec![not_a_certificate, not_a_certificate],
                vec![],
                "This machine's certificate store gave no certificate authority that can be used \
                 (none of the 2 found there could be read).",
            ),
        ];
        for (named, certificates, errors, starts_with) in cases {
            let (roots, problem) = usable_roots(certificates.into_iter(), &errors, &named);
            assert!(roots.is_empty());
            let problem = problem.expect("a problem to say");
            assert!(problem.starts_with(starts_with), "{problem}");
            assert!(
                problem.ends_with(
                    "Only the certificate authorities bundled into this program are trusted: \
                     calls that pass through an HTTPS-inspecting proxy will fail until that is \
                     put right."
                ),
                "{problem}"
            );
        }
    }
}
