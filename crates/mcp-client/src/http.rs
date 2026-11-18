//! HTTP trust for API traffic. Bundled roots are always enabled. Linux can
//! add its PEM store and all platforms can add explicitly configured PEMs.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustls::pki_types::{pem::PemObject, CertificateDer};

type Named = (Option<OsString>, Option<OsString>);

struct MachineRoots {
    named: Named,
    roots: Arc<[reqwest::Certificate]>,
}

static MACHINE_ROOTS: Mutex<Option<MachineRoots>> = Mutex::new(None);

pub fn client_builder() -> reqwest::ClientBuilder {
    machine_roots()
        .iter()
        .cloned()
        .fold(reqwest::Client::builder(), |builder, root| {
            builder.add_root_certificate(root)
        })
}

pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("an HTTP client with default settings")
}

#[derive(Debug)]
enum Source {
    File(PathBuf),
    Directory(PathBuf, bool), // system directories use OpenSSL subject-hash names
}

fn root_sources(platform: &str, named: &Named) -> Vec<Source> {
    let mut sources = Vec::new();
    #[cfg(target_os = "linux")]
    if platform == "linux" {
        // System bundles used by Debian, Red Hat, SUSE, OpenELEC and Alpine.
        // Probe independently of the environment: explicit roots are additive.
        let bundle = [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/ca-bundle.pem",
            "/etc/pki/tls/cacert.pem",
            "/etc/ssl/cert.pem",
            "/opt/etc/ssl/certs/ca-certificates.crt",
            "/etc/ssl/certs/cacert.pem",
        ]
        .into_iter()
        .map(Path::new)
        .find(|p| p.is_file());
        sources.extend(bundle.map(|p| Source::File(p.into())));
        sources.extend(
            openssl_probe::candidate_cert_dirs().map(|p| Source::Directory(p.into(), true)),
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = platform;
    if let Some(file) = &named.0 {
        sources.push(Source::File(file.into()));
    }
    if let Some(dirs) = &named.1 {
        sources.extend(std::env::split_paths(dirs).map(|p| Source::Directory(p, false)));
    }
    sources
}

fn system_cert_name(path: &Path) -> bool {
    let Some((hash, index)) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|s| s.split_once('.'))
    else {
        return false;
    };
    hash.len() == 8
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Default)]
struct Loader {
    seen: HashSet<Vec<u8>>,
    roots: Vec<reqwest::Certificate>,
    first_problem: Option<String>,
}

impl Loader {
    fn problem(&mut self, path: &Path, why: impl std::fmt::Display) {
        self.first_problem
            .get_or_insert_with(|| format!("{} ({why})", path.display()));
    }

    fn load_file(&mut self, path: &Path) -> io::Result<()> {
        // A configured path may point to a device or pipe. Never wait on one.
        let mut options = File::options();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("expected a regular PEM file"));
        }
        const MAX_BYTES: u64 = 16 * 1024 * 1024;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::other("PEM file exceeds 16 MiB"));
        }
        let mut found = false;
        for entry in CertificateDer::pem_slice_iter(&bytes) {
            found = true;
            match entry {
                Ok(der) if self.seen.contains(der.as_ref()) => {}
                Ok(der) => {
                    // reqwest parses each root when building the client. Check
                    // with the same parser so one bad entry cannot break it.
                    if rustls::RootCertStore::empty().add(der.clone()).is_ok() {
                        if let Ok(root) = reqwest::Certificate::from_der(&der) {
                            self.seen.insert(der.to_vec());
                            self.roots.push(root);
                            continue;
                        }
                    }
                    self.problem(path, "invalid certificate");
                }
                Err(e) => self.problem(path, e),
            }
        }
        if !found {
            return Err(io::Error::other("no PEM certificates"));
        }
        Ok(())
    }

    fn file(&mut self, path: &Path) {
        if let Err(e) = self.load_file(path) {
            self.problem(path, e);
        }
    }

    fn directory(&mut self, path: &Path, hashed: bool) -> io::Result<()> {
        let mut found = false;
        for entry in path.read_dir()? {
            match entry {
                Ok(entry) => {
                    let p = entry.path();
                    if (!hashed || system_cert_name(&p)) && !p.is_dir() {
                        found = true;
                        self.file(&p);
                    }
                }
                Err(e) => self.problem(path, e),
            }
        }
        if !hashed && !found {
            return Err(io::Error::other("no PEM files"));
        }
        Ok(())
    }
}

fn machine_roots() -> Arc<[reqwest::Certificate]> {
    let named = (
        std::env::var_os("SSL_CERT_FILE"),
        std::env::var_os("SSL_CERT_DIR"),
    );
    let mut held = MACHINE_ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = held.as_ref().filter(|entry| entry.named == named) {
        return cached.roots.clone();
    }
    let mut loader = Loader::default();
    for source in root_sources(std::env::consts::OS, &named) {
        match source {
            Source::File(path) => loader.file(&path),
            Source::Directory(path, hashed) => {
                if let Err(e) = loader.directory(&path, hashed) {
                    loader.problem(&path, e);
                }
            }
        }
    }
    if let Some(problem) = loader.first_problem {
        eprintln!("Warning: skipped unusable CA data at {problem}. Check the certificate store, SSL_CERT_FILE and SSL_CERT_DIR. Bundled roots remain trusted.");
    }
    let roots: Arc<[_]> = loader.roots.into();
    *held = Some(MachineRoots {
        named,
        roots: roots.clone(),
    });
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_linux_reads_a_system_store_without_an_explicit_pem() {
        for platform in ["macos", "windows"] {
            assert!(root_sources(platform, &(None, None)).is_empty());
        }
    }
}
