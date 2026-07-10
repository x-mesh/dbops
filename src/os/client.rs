//! Shared OpenSearch connection layer. `os::client::connect` is the single
//! entry point every `os` subcommand (this task's `health`/`nodes`, and
//! later `indices`/`shards`/`stats`/`init`/`reset`/`seed`) builds its
//! [`opensearch::OpenSearch`] client through.
//!
//! ## TLS / `--insecure` constraint
//!
//! `Cargo.toml` pins `opensearch = { version = "2.4", default-features =
//! false }` with neither the `native-tls` nor `rustls-tls` feature enabled
//! (see `docs/build-spike.md`): both of opensearch-rs's TLS features forward
//! to `reqwest`'s `rustls` feature, which forces the `aws-lc-rs` crypto
//! backend this workspace deliberately avoids everywhere (musl cross-build
//! regression). One consequence, confirmed by reading opensearch-rs 2.4.0's
//! source directly: `opensearch::cert::CertificateValidation` — the type
//! that lets a caller disable certificate verification — only exists behind
//! `#[cfg(any(feature = "native-tls", feature = "rustls-tls"))]`, and
//! `TransportBuilder::build()` only *reads* a configured `cert_validation`
//! value inside that same `#[cfg]` block. With neither feature enabled,
//! there is no code path in this binary that can skip TLS certificate
//! validation.
//!
//! `https://` hosts still work (feature unification means the shared
//! `reqwest` TLS connector this workspace already builds in via its own
//! `rustls-no-provider` edge is available to opensearch's HTTP client too —
//! see the "Deviation 2" note in `docs/build-spike.md`), just always with
//! full, default certificate validation. `connect` therefore rejects
//! `--insecure` against any `https://` host up front with an explicit error
//! rather than silently ignoring the flag.

use std::fmt::Debug;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use opensearch::auth::Credentials;
use opensearch::http::transport::{Connection, ConnectionPool, TransportBuilder};
use opensearch::http::Url;
use opensearch::OpenSearch;

use crate::frame::config::OpenSearchProfile;

/// Round-robins across every configured host.
///
/// opensearch-rs 2.4 only ships [`opensearch::http::transport::SingleNodeConnectionPool`]
/// out of the box — no multi-node pool — which doesn't fit the 3-node
/// cluster this toolkit targets. This fills that gap. It does not probe
/// liveness: a request against a downed node still fails (the caller sees
/// that failure), it just rotates to a different host on the *next* call,
/// so a single bad node in the list doesn't wedge every subsequent command.
///
/// Precondition: `connections` is non-empty. [`connect`] is the only
/// constructor and it validates `profile.hosts` is non-empty before this
/// type is ever built.
#[derive(Debug, Clone)]
struct RoundRobinConnectionPool {
    connections: Vec<Connection>,
    cursor: Arc<AtomicUsize>,
}

impl RoundRobinConnectionPool {
    fn new(urls: Vec<Url>) -> Self {
        Self {
            connections: urls.into_iter().map(Connection::new).collect(),
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ConnectionPool for RoundRobinConnectionPool {
    fn next(&self) -> Connection {
        let i = self.cursor.fetch_add(1, Ordering::Relaxed) % self.connections.len();
        self.connections[i].clone()
    }
}

/// Build a connected [`opensearch::OpenSearch`] client from a resolved
/// profile. Building the client never itself makes a network call —
/// `TransportBuilder::build()` only constructs a `reqwest::Client` — so a
/// bad host is only discovered once the caller sends a request. `timeout`
/// is set on the underlying HTTP client as a defense-in-depth default;
/// callers making individual requests should still wrap `.send()` in
/// `tokio::time::timeout(ctx.timeout, ...)` for a deadline that's
/// enforced regardless of how the HTTP client itself behaves at the edges
/// (DNS resolution, connect, TLS handshake).
pub fn connect(
    profile: &OpenSearchProfile,
    timeout: Duration,
    insecure: bool,
) -> Result<OpenSearch> {
    if profile.hosts.is_empty() {
        bail!("no opensearch hosts configured (set [profiles.<name>.opensearch] hosts, or DBOPS_OS_HOSTS)");
    }

    let urls = profile
        .hosts
        .iter()
        .map(|host| {
            let url = Url::parse(host).with_context(|| format!("invalid opensearch host url: {host:?}"))?;
            match url.scheme() {
                "http" | "https" => Ok(url),
                other => bail!("opensearch host {host:?} has unsupported scheme {other:?} (expected http or https)"),
            }
        })
        .collect::<Result<Vec<Url>>>()?;

    if insecure && urls.iter().any(|u| u.scheme() == "https") {
        bail!(
            "--insecure is not supported for https opensearch hosts in this build: opensearch-rs's \
             certificate-validation override requires its native-tls/rustls-tls feature, which this \
             binary intentionally does not enable (see docs/build-spike.md and os::client's module docs). \
             Use http:// hosts, or a certificate the system trust store already accepts."
        );
    }

    let credentials = match (&profile.username, &profile.password) {
        (Some(user), Some(pass)) => Some(Credentials::Basic(
            user.expose().to_string(),
            pass.expose().to_string(),
        )),
        (None, None) => None,
        _ => bail!("opensearch basic auth requires both username and password to be set"),
    };

    let pool = RoundRobinConnectionPool::new(urls);
    let mut builder = TransportBuilder::new(pool).timeout(timeout);
    if let Some(creds) = credentials {
        builder = builder.auth(creds);
    }

    let transport = builder
        .build()
        .context("failed to build opensearch transport")?;
    Ok(OpenSearch::new(transport))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::secret::Secret;

    fn profile(hosts: &[&str]) -> OpenSearchProfile {
        // reqwest's ClientBuilder needs a process-wide rustls CryptoProvider;
        // main() installs it for the binary, but the test harness does not.
        let _ = rustls::crypto::ring::default_provider().install_default();
        OpenSearchProfile {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            username: None,
            password: None,
        }
    }

    #[test]
    fn empty_hosts_is_an_error() {
        let err = connect(&profile(&[]), Duration::from_secs(5), false).unwrap_err();
        assert!(err.to_string().contains("no opensearch hosts configured"));
    }

    #[test]
    fn invalid_url_is_an_error() {
        let err = connect(&profile(&["not a url"]), Duration::from_secs(5), false).unwrap_err();
        assert!(err.to_string().contains("invalid opensearch host url"));
    }

    #[test]
    fn unsupported_scheme_is_an_error() {
        let err = connect(
            &profile(&["ftp://os.internal:9200"]),
            Duration::from_secs(5),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsupported scheme"));
    }

    #[test]
    fn insecure_with_https_host_is_rejected() {
        let err = connect(
            &profile(&["https://os.internal:9200"]),
            Duration::from_secs(5),
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--insecure is not supported"));
    }

    #[test]
    fn insecure_with_http_only_hosts_is_fine() {
        let result = connect(
            &profile(&["http://os.internal:9200"]),
            Duration::from_secs(5),
            true,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn valid_http_host_connects() {
        let result = connect(
            &profile(&["http://localhost:9200"]),
            Duration::from_secs(5),
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn multiple_valid_hosts_connect() {
        let hosts = ["http://os-1:9200", "http://os-2:9200", "http://os-3:9200"];
        let result = connect(&profile(&hosts), Duration::from_secs(5), false);
        assert!(result.is_ok());
    }

    #[test]
    fn partial_basic_auth_is_an_error() {
        let mut p = profile(&["http://localhost:9200"]);
        p.username = Some(Secret::from("user".to_string()));
        let err = connect(&p, Duration::from_secs(5), false).unwrap_err();
        assert!(err
            .to_string()
            .contains("requires both username and password"));
    }

    #[test]
    fn full_basic_auth_connects() {
        let mut p = profile(&["http://localhost:9200"]);
        p.username = Some(Secret::from("user".to_string()));
        p.password = Some(Secret::from("pass".to_string()));
        let result = connect(&p, Duration::from_secs(5), false);
        assert!(result.is_ok());
    }

    // --- RoundRobinConnectionPool ---------------------------------------------

    #[test]
    fn round_robin_pool_cycles_through_every_host() {
        let urls: Vec<Url> = ["http://a:9200", "http://b:9200", "http://c:9200"]
            .iter()
            .map(|u| Url::parse(u).unwrap())
            .collect();
        let pool = RoundRobinConnectionPool::new(urls);

        let seen: Vec<String> = (0..6).map(|_| format!("{:?}", pool.next())).collect();
        assert_eq!(seen[0], seen[3]);
        assert_eq!(seen[1], seen[4]);
        assert_eq!(seen[2], seen[5]);
        assert_ne!(seen[0], seen[1]);
        assert_ne!(seen[1], seen[2]);
    }

    #[test]
    fn round_robin_pool_single_host_always_returns_it() {
        let urls: Vec<Url> = vec![Url::parse("http://only:9200").unwrap()];
        let pool = RoundRobinConnectionPool::new(urls);
        let first = format!("{:?}", pool.next());
        let second = format!("{:?}", pool.next());
        assert_eq!(first, second);
    }
}
