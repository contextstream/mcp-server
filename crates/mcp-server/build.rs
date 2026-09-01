#[path = "build_metadata.rs"]
mod build_metadata;

use std::env;

fn unicode_env(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => panic!("{name} must contain valid Unicode"),
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=CONTEXTSTREAM_RELEASE_BUILD");
    println!(
        "cargo:rustc-env=CONTEXTSTREAM_BUILD_DATE={}",
        build_metadata::source_build_date(
            unicode_env("SOURCE_DATE_EPOCH").as_deref(),
            unicode_env("CONTEXTSTREAM_RELEASE_BUILD").as_deref(),
        )
        .unwrap_or_else(|error| panic!("{error}"))
    );
}
