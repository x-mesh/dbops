//! Shared MongoDB connection entrypoint. Every mongo subcommand (and, later,
//! `t20`/`t12`'s stats/init/reset/seed commands) builds its `mongodb::Client`
//! through [`connect`] rather than constructing one directly, so the
//! timeout/TLS wiring only needs to be right in one place.

use std::time::Duration;

use anyhow::{Context, Result};
use mongodb::options::{ClientOptions, Tls};
use mongodb::Client;

use crate::frame::config::MongoProfile;

/// Build a `mongodb::Client` from a resolved profile.
///
/// This does not itself perform any network I/O — the driver connects
/// lazily on the first operation — so callers that need a hard wall-clock
/// bound on the *first* command (ping, `replSetGetStatus`, ...) should wrap
/// that call in `tokio::time::timeout` themselves. `timeout` is still worth
/// passing here: it becomes `server_selection_timeout`, which bounds how
/// long the driver's internal server-selection loop can spin before it
/// surfaces a "no server available" error to that first operation.
pub async fn connect(profile: &MongoProfile, timeout: Duration, insecure: bool) -> Result<Client> {
    let uri = profile
        .uri
        .as_ref()
        .context("no mongodb uri configured (set DBOPS_MONGO_URI or [profiles.<name>.mongodb] uri in the config file)")?
        .expose();

    let mut options = ClientOptions::parse(uri)
        .await
        .context("failed to parse mongodb connection string")?;
    options.server_selection_timeout = Some(timeout);

    // `--insecure` only relaxes certificate verification on a connection
    // that already requested TLS (via `tls=true`/`mongodb+srv://` in the
    // URI) — it must never turn TLS on for a URI that didn't ask for it.
    if insecure {
        if let Some(Tls::Enabled(tls_opts)) = options.tls.as_mut() {
            tls_opts.allow_invalid_certificates = Some(true);
        }
    }

    Client::with_options(options).context("failed to build mongodb client")
}
