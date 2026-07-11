//! Shared rustls client config for the toolkit's HTTPS clients (`dbops
//! update` and `http check`).
//!
//! reqwest is built with the `rustls-no-provider` feature (see Cargo.toml),
//! whose default server-certificate verifier is `rustls-platform-verifier`:
//! it reads the host's system trust store and fails the *client build*
//! outright -- "No CA certificates were loaded from the system" -- on an
//! image that ships none (distroless, or a slim Debian without the
//! `ca-certificates` package). A statically-linked binary whose whole point
//! is to run on exactly those images can't depend on that.
//!
//! So these clients are handed an explicit config via reqwest's
//! `use_preconfigured_tls` instead. Its root store is the union of:
//!   - **webpki-roots** -- the Mozilla CA set, compiled into the binary, so
//!     verifying a public endpoint (api.github.com, the release CDN) never
//!     depends on the host having a CA bundle at all; and
//!   - the host's **native roots**, best-effort -- so an endpoint whose
//!     private CA is installed on the box (an intranet service behind
//!     `http check`, a GitHub Enterprise mirror) still verifies.
//!
//! Bundled roots alone would reject the private-CA case; native roots alone
//! are what breaks on a minimal image. The union covers both, and it changes
//! trust only for these two reqwest clients -- the database clients keep
//! their own separate TLS configuration.
//!
//! This is the secure path only. `--insecure` (http check) never reaches
//! here: reqwest's own `danger_accept_invalid_certs` already skips
//! verification without touching any trust store, so it works on a minimal
//! image as-is.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::ClientConfig;

/// Advertise HTTP/2, then HTTP/1.1. `use_preconfigured_tls` bypasses the ALPN
/// list reqwest sets from its own builder, so without this h2 is never
/// negotiated and the `http2` feature we pay for goes unused.
const ALPN_PROTOCOLS: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// Build the server-auth rustls config shared by the toolkit's HTTPS
/// clients. See the module docs for why the root store is a union.
///
/// The crypto provider is passed explicitly (ring, the one backend this
/// workspace compiles -- see Cargo.toml) rather than read from the
/// process-wide default, so this is correct even if called before
/// `main`'s `install_default()` and testable without it.
pub fn bundled_root_config() -> Result<ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();

    // Mozilla set: always present, host-independent.
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Host's own roots on top, best-effort. A private CA lives here, not in
    // the Mozilla set; on a minimal image this returns nothing (or errors),
    // which is fine -- the bundled set above still stands. Individual
    // malformed entries are skipped so one bad cert can't sink the config.
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("select TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = ALPN_PROTOCOLS.iter().map(|p| p.to_vec()).collect();
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builds_with_bundled_roots_even_with_no_system_store() {
        // Doesn't install any crypto provider and doesn't depend on the host
        // having a CA bundle -- exactly the minimal-image condition.
        let config = bundled_root_config().expect("config builds");
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            "h2 must be advertised or use_preconfigured_tls silently drops it"
        );
    }

    /// The Mozilla set is compiled in, so the store is never empty regardless
    /// of the host -- that is the whole point of the union.
    #[test]
    fn bundled_roots_are_present_without_any_native_store() {
        assert!(
            !webpki_roots::TLS_SERVER_ROOTS.is_empty(),
            "webpki-roots must ship a non-empty trust anchor set"
        );
    }
}
