//! Postgres connection layer shared by every `pg` subcommand (and by other
//! domains that need a bare postgres probe). Every connection goes through
//! `tokio-postgres`'s own `SslMode::Prefer` (its default): the client
//! attempts TLS first and transparently falls back to a plaintext session if
//! the server doesn't advertise SSL support. That single code path is what
//! lets this module work against both TLS-terminated and plain
//! internal-network instances without a separate "try TLS, else NoTls"
//! branch.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::{Client, Config};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::frame::config::PostgresProfile;

const DEFAULT_HOST: &str = "localhost";
const DEFAULT_PORT: u16 = 5432;

/// Connect to postgres per `profile`, bounded end-to-end (TCP + TLS
/// negotiation + startup/auth) by `timeout`.
///
/// Owns its own timeout rather than assuming every caller wraps it: this is
/// the shared pg entry point other domain modules call directly, so it has
/// to be safe to call bare.
///
/// `insecure` swaps the webpki-roots certificate verifier for one that
/// accepts any server certificate (self-signed/internal CAs the local trust
/// store doesn't carry) -- TLS itself is still negotiated either way.
pub async fn connect(profile: &PostgresProfile, timeout: Duration, insecure: bool) -> Result<Client> {
    let mut config = Config::new();
    config.host(profile.host.as_deref().unwrap_or(DEFAULT_HOST));
    config.port(profile.port.unwrap_or(DEFAULT_PORT));
    if let Some(user) = &profile.user {
        config.user(user.expose());
    }
    if let Some(password) = &profile.password {
        config.password(password.expose());
    }
    if let Some(dbname) = &profile.dbname {
        config.dbname(dbname);
    }

    let connector = build_connector(insecure);

    let (client, connection) = tokio::time::timeout(timeout, config.connect(connector))
        .await
        .context("connection timed out")?
        .context("connection failed")?;

    // tokio-postgres splits the client handle from the connection driver;
    // the driver future must be polled somewhere or every query on `client`
    // hangs forever. Spawning it means a post-handshake connection drop
    // surfaces as a failure on the next query rather than through this
    // function's return value -- the same tradeoff every tokio-postgres
    // caller makes.
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            eprintln!("dbops: pg connection closed with error: {err}");
        }
    });

    Ok(client)
}

fn build_connector(insecure: bool) -> MakeRustlsConnect {
    if !insecure {
        return MakeRustlsConnect::with_webpki_roots();
    }
    let mut tls_config = ClientConfig::builder()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    tls_config
        .dangerous()
        .set_certificate_verifier(Arc::new(AcceptAnyCert));
    MakeRustlsConnect::new(tls_config)
}

/// `--insecure`'s certificate verifier: accepts any server certificate
/// (self-signed, expired, hostname mismatch, ...). Only the peer-identity
/// check is skipped -- the TLS handshake, encryption, and channel binding
/// still run for real.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // Mirror the process's own crypto provider rather than hand-picking
        // a subset of schemes, so --insecure negotiates the same cipher
        // surface a verified connection would.
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_connector_insecure_does_not_panic() {
        // Exercises the ClientConfig/verifier wiring without a network
        // connection -- construction alone catches API misuse (wrong
        // builder state, missing provider) that would otherwise only
        // surface the first time `--insecure` is used against a live
        // server.
        let _ = build_connector(true);
    }

    #[test]
    fn build_connector_secure_does_not_panic() {
        let _ = build_connector(false);
    }
}
