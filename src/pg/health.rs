//! `dbops pg health`: connect, tell primary from standby, and report replica
//! lag against `--warning`/`--critical` thresholds. Never returns a bare
//! `Result`: connect failures and timeouts become an `Unknown` result
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
/// can hit. Callers must check this before calling [`health`], which stays
/// infallible.
pub fn parse_args(args: &HealthArgs) -> Result<(Option<Duration>, Option<Duration>)> {
    let warning = parse_threshold(args.warning.as_deref()).context("invalid --warning value")?;
    let critical = parse_threshold(args.critical.as_deref()).context("invalid --critical value")?;
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

    let replication = if in_recovery {
        Replication::Standby {
            lag_seconds: standby_lag_seconds(&pg_client).await?,
        }
    } else {
        let (connected_standbys, lag_seconds) = primary_replication(&pg_client).await?;
        Replication::Primary {
            connected_standbys,
            lag_seconds,
        }
    };
    let lag_seconds = replication.lag_seconds();

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
        summary: build_summary(&replication),
        metrics,
    })
}

/// This instance's replication role, plus what each role needs to describe
/// its lag. The two ways lag can be absent are deliberately distinct: a
/// primary tracks how many standbys are attached, so "connected but caught
/// up" (a healthy quiet cluster) never gets misreported as "no replicas".
#[derive(Debug, Clone, PartialEq)]
enum Replication {
    /// `lag_seconds` is the worst `replay_lag` across `connected_standbys`
    /// rows in `pg_stat_replication`. It is `None`, not `0`, whenever no
    /// standby has outstanding WAL to replay: `replay_lag` is `NULL` until
    /// there is un-replayed WAL, so a fully caught-up (or briefly idle)
    /// replica reports no measurable lag even while streaming.
    Primary {
        connected_standbys: i64,
        lag_seconds: Option<f64>,
    },
    /// `lag_seconds` is how far behind the last replayed transaction is,
    /// `None` before anything has been replayed yet.
    Standby { lag_seconds: Option<f64> },
}

impl Replication {
    /// The single lag value fed to the metric and threshold logic, whichever
    /// role produced it.
    fn lag_seconds(&self) -> Option<f64> {
        match self {
            Replication::Primary { lag_seconds, .. } | Replication::Standby { lag_seconds } => {
                *lag_seconds
            }
        }
    }
}

/// Delegates to the shared [`frame::health::parse_threshold`]. Unlike
/// `frame::ctx::parse_timeout` (`--timeout`-only, forbids `0`), `0` is a
/// valid lag threshold here. A bare number with no `ms`/`s`/`m` suffix is
/// treated as a plain seconds count, matching this domain's lag semantics.
fn parse_threshold(raw: Option<&str>) -> Result<Option<Duration>> {
    raw.map(|v| health::parse_threshold(v).map(|t| Duration::from_secs_f64(t.as_seconds())))
        .transpose()
}

/// No lag metric (single-instance primary with no replicas, or a standby
/// that hasn't replayed anything yet) means there's nothing to compare
/// against a threshold. That's `Ok`, not "threshold trivially satisfied"
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

fn build_summary(replication: &Replication) -> String {
    match replication {
        // A measurable lag is the headline whenever there is one.
        Replication::Primary {
            lag_seconds: Some(lag),
            ..
        } => format!("primary, replica lag {lag:.1}s"),
        // No standby row at all: a genuinely lone primary.
        Replication::Primary {
            connected_standbys: 0,
            ..
        } => "primary, no replicas connected".to_string(),
        // Standbys are streaming, they are simply caught up: replay_lag is
        // NULL. Distinct from the line above so a healthy quiet cluster
        // isn't misread as having lost its replicas.
        Replication::Primary {
            connected_standbys,
            lag_seconds: None,
        } => format!("primary, {connected_standbys} replica(s) connected, no measurable lag"),
        Replication::Standby {
            lag_seconds: Some(lag),
        } => format!("standby, replay lag {lag:.1}s"),
        Replication::Standby { lag_seconds: None } => "standby, replay lag unknown".to_string(),
    }
}

/// Primary-side view: how many standbys are streaming, and the worst
/// (largest) `replay_lag` across them, per `pg_stat_replication`.
///
/// The count and the lag answer two different questions. The count is `0`
/// only when no standby is attached. The lag is `NULL` (reported as `None`,
/// never a fabricated `0`) whenever no attached standby has un-replayed WAL,
/// which includes the common healthy case of a caught-up replica on a
/// quiet cluster. Reading them together is what lets the summary tell "no
/// replicas" apart from "replicas connected, nothing to replay".
async fn primary_replication(pg_client: &Client) -> Result<(i64, Option<f64>)> {
    let row = pg_client
        .query_one(
            "SELECT count(*)::int8, EXTRACT(EPOCH FROM MAX(replay_lag))::float8 \
             FROM pg_stat_replication",
            &[],
        )
        .await
        .context("pg_stat_replication query failed")?;
    let connected_standbys: i64 = row
        .try_get(0)
        .context("unexpected pg_stat_replication row shape")?;
    let lag_seconds: Option<f64> = row
        .try_get(1)
        .context("unexpected pg_stat_replication row shape")?;
    Ok((connected_standbys, lag_seconds))
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

/// Informational only: never fails the health check. A role without
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

/// Best-effort, stderr-only: an old server is still worth checking, just
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
        assert_eq!(parse_threshold(Some("0s")).unwrap(), Some(Duration::ZERO));
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
            build_summary(&Replication::Primary {
                connected_standbys: 1,
                lag_seconds: Some(0.3),
            }),
            "primary, replica lag 0.3s"
        );
    }

    #[test]
    fn summary_primary_without_replicas() {
        assert_eq!(
            build_summary(&Replication::Primary {
                connected_standbys: 0,
                lag_seconds: None,
            }),
            "primary, no replicas connected"
        );
    }

    /// The case that broke CI: a replica IS streaming but has caught up, so
    /// replay_lag is NULL. This must not read as "no replicas connected".
    #[test]
    fn summary_primary_with_caught_up_replica_is_not_reported_as_no_replicas() {
        let summary = build_summary(&Replication::Primary {
            connected_standbys: 2,
            lag_seconds: None,
        });
        assert_eq!(
            summary,
            "primary, 2 replica(s) connected, no measurable lag"
        );
        assert!(!summary.contains("no replicas connected"));
    }

    #[test]
    fn summary_standby_with_lag() {
        assert_eq!(
            build_summary(&Replication::Standby {
                lag_seconds: Some(1.25),
            }),
            "standby, replay lag 1.2s"
        );
    }

    #[test]
    fn summary_standby_unknown_lag() {
        assert_eq!(
            build_summary(&Replication::Standby { lag_seconds: None }),
            "standby, replay lag unknown"
        );
    }

    /// The lag that feeds the metric/threshold logic is the same value
    /// regardless of which role produced it.
    #[test]
    fn lag_seconds_accessor_reads_either_role() {
        assert_eq!(
            Replication::Primary {
                connected_standbys: 1,
                lag_seconds: Some(2.0),
            }
            .lag_seconds(),
            Some(2.0)
        );
        assert_eq!(
            Replication::Standby {
                lag_seconds: Some(3.0)
            }
            .lag_seconds(),
            Some(3.0)
        );
        assert_eq!(
            Replication::Primary {
                connected_standbys: 0,
                lag_seconds: None,
            }
            .lag_seconds(),
            None
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
