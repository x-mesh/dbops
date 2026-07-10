//! `dbops pg health`: connect, tell primary from standby, and report replica
//! lag against `--warning`/`--critical` thresholds. Never returns a bare
//! `Result` -- connect failures and timeouts become an `Unknown` result
//! rather than an early `Err`, so the nagios line always prints and the exit
//! code always comes from [`frame::exit::from_status`], even when postgres
//! is unreachable.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::frame::health;
use crate::frame::result::{CheckResult, CheckStatus, Metric};
use crate::frame::{Ctx, HealthArgs};
use crate::pg::client;

/// PG 13 is the oldest version this toolkit's queries are verified against
/// (`replay_lag` in `pg_stat_replication` and `pg_last_xact_replay_timestamp`
/// both predate it, but PRD R35 only commits to 13+).
const MIN_SUPPORTED_MAJOR_VERSION: u32 = 13;

/// Parse `--warning`/`--critical` up front. A bad value is a usage error
/// (exit 3, no nagios line), distinct from every other failure this check
/// can hit -- callers must check this before calling [`health`], which stays
/// infallible.
pub fn parse_args(args: &HealthArgs) -> Result<(Option<Duration>, Option<Duration>)> {
    let warning = parse_threshold(args.warning.as_deref()).context("invalid --warning value")?;
    let critical =
        parse_threshold(args.critical.as_deref()).context("invalid --critical value")?;
    Ok((warning, critical))
}

pub async fn health(
    ctx: &Ctx,
    args: &HealthArgs,
    warning: Option<Duration>,
    critical: Option<Duration>,
) -> CheckResult {
    match tokio::time::timeout(ctx.timeout, run(ctx, args, warning, critical)).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => unknown(format!("{err:#}")),
        Err(_) => unknown(format!("timed out after {:?}", ctx.timeout)),
    }
}

fn unknown(reason: String) -> CheckResult {
    CheckResult {
        status: CheckStatus::Unknown,
        summary: reason,
        metrics: Vec::new(),
    }
}

async fn run(
    ctx: &Ctx,
    args: &HealthArgs,
    warning: Option<Duration>,
    critical: Option<Duration>,
) -> Result<CheckResult> {
    let pg_client = client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await?;

    warn_if_unsupported_version(&pg_client).await;

    let in_recovery: bool = pg_client
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await
        .context("pg_is_in_recovery() query failed")?
        .try_get(0)
        .context("unexpected pg_is_in_recovery() row shape")?;

    let (role, lag_seconds) = if in_recovery {
        (Role::Standby, standby_lag_seconds(&pg_client).await?)
    } else {
        (Role::Primary, primary_lag_seconds(&pg_client).await?)
    };

    let mut metrics = Vec::new();
    if let Some(lag) = lag_seconds {
        metrics.push(Metric {
            name: "lag".to_string(),
            value: lag,
            unit: Some("s".to_string()),
            warn: args.warning.clone(),
            crit: args.critical.clone(),
        });
    }
    if let Some((used, max)) = connection_counts(&pg_client).await {
        metrics.push(Metric {
            name: "connections".to_string(),
            value: used as f64,
            unit: None,
            warn: None,
            crit: None,
        });
        metrics.push(Metric {
            name: "max_connections".to_string(),
            value: max as f64,
            unit: None,
            warn: None,
            crit: None,
        });
    }

    Ok(CheckResult {
        status: evaluate_status(lag_seconds, warning, critical),
        summary: build_summary(role, lag_seconds),
        metrics,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Primary,
    Standby,
}

/// Delegates to the shared [`frame::health::parse_threshold`] -- unlike
/// `frame::ctx::parse_timeout` (`--timeout`-only, forbids `0`), `0` is a
/// valid lag threshold here. A bare number with no `ms`/`s`/`m` suffix is
/// treated as a plain seconds count, matching this domain's lag semantics.
fn parse_threshold(raw: Option<&str>) -> Result<Option<Duration>> {
    raw.map(|v| health::parse_threshold(v).map(|t| Duration::from_secs_f64(t.as_seconds())))
        .transpose()
}

/// No lag metric (single-instance primary with no replicas, or a standby
/// that hasn't replayed anything yet) means there's nothing to compare
/// against a threshold -- that's `Ok`, not "threshold trivially satisfied"
/// or "unknown".
fn evaluate_status(
    lag_seconds: Option<f64>,
    warning: Option<Duration>,
    critical: Option<Duration>,
) -> CheckStatus {
    let Some(lag) = lag_seconds else {
        return CheckStatus::Ok;
    };
    if critical.is_some_and(|c| lag >= c.as_secs_f64()) {
        return CheckStatus::Critical;
    }
    if warning.is_some_and(|w| lag >= w.as_secs_f64()) {
        return CheckStatus::Warning;
    }
    CheckStatus::Ok
}

fn build_summary(role: Role, lag_seconds: Option<f64>) -> String {
    match (role, lag_seconds) {
        (Role::Primary, Some(lag)) => format!("primary, replica lag {lag:.1}s"),
        (Role::Primary, None) => "primary, no replicas connected".to_string(),
        (Role::Standby, Some(lag)) => format!("standby, replay lag {lag:.1}s"),
        (Role::Standby, None) => "standby, replay lag unknown".to_string(),
    }
}

/// Primary-side view: the worst (largest) `replay_lag` across every
/// connected standby, per `pg_stat_replication`. `NULL` (no rows, or every
/// row's `replay_lag` itself `NULL`) means "no replicas reporting usable
/// lag" -- reported as `None`, not `0`, so a single-instance primary doesn't
/// get a fabricated zero-lag metric.
async fn primary_lag_seconds(pg_client: &Client) -> Result<Option<f64>> {
    let row = pg_client
        .query_one(
            "SELECT EXTRACT(EPOCH FROM MAX(replay_lag))::float8 FROM pg_stat_replication",
            &[],
        )
        .await
        .context("pg_stat_replication query failed")?;
    row.try_get(0)
        .context("unexpected pg_stat_replication row shape")
}

/// Standby-side view: how far behind the last replayed transaction is from
/// now. `NULL` before anything has been replayed yet.
async fn standby_lag_seconds(pg_client: &Client) -> Result<Option<f64>> {
    let row = pg_client
        .query_one(
            "SELECT EXTRACT(EPOCH FROM (now() - pg_last_xact_replay_timestamp()))::float8",
            &[],
        )
        .await
        .context("pg_last_xact_replay_timestamp() query failed")?;
    row.try_get(0)
        .context("unexpected pg_last_xact_replay_timestamp() row shape")
}

/// Informational only -- never fails the health check. A role without
/// `pg_monitor`/superuser can still get a lag/role result even if it can't
/// see `pg_stat_activity` or `max_connections`.
async fn connection_counts(pg_client: &Client) -> Option<(i64, i64)> {
    let used: i64 = pg_client
        .query_one("SELECT count(*) FROM pg_stat_activity", &[])
        .await
        .ok()?
        .try_get(0)
        .ok()?;
    let max_raw: String = pg_client
        .query_one("SHOW max_connections", &[])
        .await
        .ok()?
        .try_get(0)
        .ok()?;
    let max: i64 = max_raw.trim().parse().ok()?;
    Some((used, max))
}

/// Best-effort, stderr-only -- an old server is still worth checking, just
/// flagged. Never affects `CheckStatus`.
async fn warn_if_unsupported_version(pg_client: &Client) {
    let Ok(row) = pg_client.query_one("SHOW server_version", &[]).await else {
        return;
    };
    let Ok(version) = row.try_get::<_, String>(0) else {
        return;
    };
    let major = version
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|s| s.parse::<u32>().ok());
    if major.is_some_and(|m| m < MIN_SUPPORTED_MAJOR_VERSION) {
        eprintln!("warning: postgres server_version {version} is older than {MIN_SUPPORTED_MAJOR_VERSION} (unsupported)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_lag_metric_is_always_ok() {
        assert_eq!(
            evaluate_status(
                None,
                Some(Duration::from_millis(1)),
                Some(Duration::from_millis(1))
            ),
            CheckStatus::Ok
        );
    }

    #[test]
    fn lag_below_every_threshold_is_ok() {
        assert_eq!(
            evaluate_status(
                Some(0.1),
                Some(Duration::from_secs(5)),
                Some(Duration::from_secs(30))
            ),
            CheckStatus::Ok
        );
    }

    #[test]
    fn lag_at_or_above_warning_is_warning() {
        assert_eq!(
            evaluate_status(
                Some(5.0),
                Some(Duration::from_secs(5)),
                Some(Duration::from_secs(30))
            ),
            CheckStatus::Warning
        );
    }

    #[test]
    fn lag_at_or_above_critical_is_critical_even_under_warning() {
        assert_eq!(
            evaluate_status(
                Some(31.0),
                Some(Duration::from_secs(5)),
                Some(Duration::from_secs(30))
            ),
            CheckStatus::Critical
        );
    }

    #[test]
    fn critical_takes_priority_when_both_thresholds_are_crossed() {
        assert_eq!(
            evaluate_status(
                Some(100.0),
                Some(Duration::from_secs(5)),
                Some(Duration::from_secs(30))
            ),
            CheckStatus::Critical
        );
    }

    #[test]
    fn no_thresholds_set_is_always_ok() {
        assert_eq!(evaluate_status(Some(9999.0), None, None), CheckStatus::Ok);
    }

    #[test]
    fn parse_threshold_none_is_none() {
        assert_eq!(parse_threshold(None).unwrap(), None);
    }

    #[test]
    fn parse_threshold_rejects_garbage() {
        assert!(parse_threshold(Some("not-a-duration")).is_err());
    }

    #[test]
    fn parse_threshold_accepts_zero() {
        assert_eq!(
            parse_threshold(Some("0s")).unwrap(),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_threshold(Some("0")).unwrap(), Some(Duration::ZERO));
    }

    #[test]
    fn parse_threshold_accepts_bare_seconds_and_suffixed_forms() {
        assert_eq!(
            parse_threshold(Some("7")).unwrap(),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            parse_threshold(Some("500ms")).unwrap(),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn summary_primary_with_lag() {
        assert_eq!(
            build_summary(Role::Primary, Some(0.3)),
            "primary, replica lag 0.3s"
        );
    }

    #[test]
    fn summary_primary_without_replicas() {
        assert_eq!(
            build_summary(Role::Primary, None),
            "primary, no replicas connected"
        );
    }

    #[test]
    fn summary_standby_with_lag() {
        assert_eq!(
            build_summary(Role::Standby, Some(1.25)),
            "standby, replay lag 1.2s"
        );
    }

    #[test]
    fn summary_standby_unknown_lag() {
        assert_eq!(
            build_summary(Role::Standby, None),
            "standby, replay lag unknown"
        );
    }

    #[test]
    fn unknown_result_has_no_metrics() {
        let result = unknown("connection refused".to_string());
        assert_eq!(result.status, CheckStatus::Unknown);
        assert_eq!(result.summary, "connection refused");
        assert!(result.metrics.is_empty());
    }
}
