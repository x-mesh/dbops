//! Shared connection + call-timeout plumbing for every `redis` subcommand.
//!
//! Every remote call in this module goes through [`connect`] (for the
//! connection handshake) or [`call`] (for a command against an already-open
//! connection) so `ctx.timeout` is applied uniformly instead of each
//! submodule reimplementing its own `tokio::time::timeout` wrapping.

use anyhow::{anyhow, Result};
use redis::aio::MultiplexedConnection;
use redis::IntoConnectionInfo;

use crate::frame::Ctx;

/// Resolve the configured `redis://`/`rediss://` URI and open a multiplexed
/// async connection, honoring `ctx.timeout` for the handshake and
/// `ctx.insecure` for TLS hostname verification.
///
/// Returns a plain `anyhow::Result`: callers decide how a failure maps
/// onto their own exit-code contract (`health` -> Unknown/exit 3, stat
/// commands -> stderr/exit 4).
pub async fn connect(ctx: &Ctx) -> Result<MultiplexedConnection> {
    let uri = ctx.profile.redis.uri.as_ref().ok_or_else(|| {
        anyhow!("redis URI not configured (set DBOPS_REDIS_URI or a config profile)")
    })?;

    let mut info = uri.expose().into_connection_info()?;
    if ctx.insecure {
        let mut addr = info.addr().clone();
        addr.set_danger_accept_invalid_hostnames(true);
        info = info.set_addr(addr);
    }

    let client = redis::Client::open(info)?;
    let conn = tokio::time::timeout(ctx.timeout, client.get_multiplexed_async_connection())
        .await
        .map_err(|_| anyhow!("redis connection timed out after {:?}", ctx.timeout))??;
    Ok(conn)
}

/// Run a single redis command future under `ctx.timeout`, mapping both a
/// timeout and a protocol/IO error onto `anyhow::Error` uniformly.
pub async fn call<T>(
    ctx: &Ctx,
    fut: impl std::future::Future<Output = redis::RedisResult<T>>,
) -> Result<T> {
    tokio::time::timeout(ctx.timeout, fut)
        .await
        .map_err(|_| anyhow!("redis command timed out after {:?}", ctx.timeout))?
        .map_err(Into::into)
}
