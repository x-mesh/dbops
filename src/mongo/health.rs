//! `dbops mongo health` — ping + `replSetGetStatus` summary.
//!
//! Decision table (PRD edge cases):
//! - standalone (no replica set): `ping` OK alone -> OK, summary says so.
//! - PRIMARY present, all members healthy -> OK.
//! - PRIMARY present, some member(s) unreachable -> WARNING.
//! - no PRIMARY -> CRITICAL.
//! - can't even connect within `ctx.timeout` -> UNKNOWN, exit 3.
//!
//! The PRIMARY/lag judgement itself lives in [`super::replset_status`] as a
//! pure function so it can be unit-tested against mock documents without a
//! live server; this module only owns the connect/ping/timeout plumbing and
//! CheckResult/exit-code wiring.

use anyhow::{Context, Result};
use mongodb::bson::{doc, Document};
use mongodb::error::ErrorKind;
use mongodb::Client;

use crate::frame::result::{CheckResult, CheckStatus, Metric};
use crate::frame::{health, Ctx, ExitCode, HealthArgs};
use crate::mongo::{client, replset_status};

/// Server error code for `replSetGetStatus` run against a node that isn't a
/// replica set member ("not running with --replSet").
const NO_REPLICA_SET_CONFIG: i32 = 76;

pub async fn run(ctx: &Ctx, args: &HealthArgs) -> Result<ExitCode> {
    // A bad --warning/--critical value is a usage error, not a connectivity
    // problem: it's handled here, directly, with a plain stderr message and
    // exit 3 -- not propagated as a bare `Err` (which would surface through
    // main.rs's generic handler as exit 1, outside the nagios UNKNOWN(3)
    // vocabulary every other failure mode in this check uses).
    let (warning, critical) = match parse_args(args) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(crate::frame::exit::unix::ARGUMENT_ERROR));
        }
    };

    let result = check(ctx, warning, critical).await;
    println!(
        "{}",
        crate::frame::output::render_check("mongo", "health", &result, ctx.json)
    );
    Ok(ExitCode::from(crate::frame::exit::from_status(
        result.status,
    )))
}

fn parse_args(args: &HealthArgs) -> Result<(Option<i64>, Option<i64>)> {
    let warning = args
        .warning
        .as_deref()
        .map(parse_lag_seconds)
        .transpose()
        .context("invalid --warning value")?;
    let critical = args
        .critical
        .as_deref()
        .map(parse_lag_seconds)
        .transpose()
        .context("invalid --critical value")?;
    Ok((warning, critical))
}

enum ReplStatusOutcome {
    Standalone,
    ReplicaSet(Document),
}

/// Run the probe and turn every outcome — success, standalone, or any
/// failure short of a malformed CLI flag — into a [`CheckResult`]. Never
/// returns `Err`: a connect/ping/timeout failure is itself a reportable
/// UNKNOWN result, not a process-level error.
async fn check(ctx: &Ctx, warning: Option<i64>, critical: Option<i64>) -> CheckResult {
    match tokio::time::timeout(ctx.timeout, probe(ctx)).await {
        Err(_elapsed) => unknown(format!(
            "mongodb health check did not complete within {:?}",
            ctx.timeout
        )),
        Ok(Err(err)) => unknown(format!("mongodb health check failed: {err:#}")),
        Ok(Ok(ReplStatusOutcome::Standalone)) => CheckResult {
            status: CheckStatus::Ok,
            summary: "standalone (no replica set)".to_string(),
            metrics: vec![],
        },
        Ok(Ok(ReplStatusOutcome::ReplicaSet(doc))) => {
            build_replicaset_result(&doc, warning, critical)
        }
    }
}

async fn probe(ctx: &Ctx) -> Result<ReplStatusOutcome> {
    let mongo_client: Client =
        client::connect(&ctx.profile.mongodb, ctx.timeout, ctx.insecure).await?;
    let admin = mongo_client.database("admin");

    admin
        .run_command(doc! { "ping": 1 })
        .await
        .context("ping failed")?;

    match admin.run_command(doc! { "replSetGetStatus": 1 }).await {
        Ok(status_doc) => Ok(ReplStatusOutcome::ReplicaSet(status_doc)),
        Err(err) if is_not_replica_set_error(&err) => Ok(ReplStatusOutcome::Standalone),
        Err(err) => Err(err).context("replSetGetStatus failed"),
    }
}

fn is_not_replica_set_error(err: &mongodb::error::Error) -> bool {
    matches!(
        err.kind.as_ref(),
        ErrorKind::Command(cmd) if cmd.code == NO_REPLICA_SET_CONFIG || cmd.code_name == "NoReplicaSetConfig"
    )
}

fn build_replicaset_result(
    doc: &Document,
    warning: Option<i64>,
    critical: Option<i64>,
) -> CheckResult {
    let members = match replset_status::parse_members(doc) {
        Ok(members) => members,
        Err(err) => {
            return unknown(format!(
                "failed to parse replSetGetStatus response: {err:#}"
            ))
        }
    };

    let judgement = replset_status::judge_replset(&members, warning, critical);

    let mut metrics = Vec::new();
    if let Some(lag) = judgement.max_lag_secs {
        metrics.push(Metric {
            name: "replset_lag_max".to_string(),
            value: lag as f64,
            unit: Some("s".to_string()),
            warn: warning.map(|w| w.to_string()),
            crit: critical.map(|c| c.to_string()),
        });
    }
    metrics.push(Metric {
        name: "replset_members_unhealthy".to_string(),
        value: judgement.unhealthy_count as f64,
        unit: None,
        warn: None,
        crit: None,
    });

    CheckResult {
        status: judgement.status,
        summary: judgement.summary,
        metrics,
    }
}

fn unknown(summary: String) -> CheckResult {
    CheckResult {
        status: CheckStatus::Unknown,
        summary,
        metrics: vec![],
    }
}

/// Parse a `--warning`/`--critical` lag threshold via the shared duration/
/// count parser (`0` allowed -- "alert on any lag at all"). A bare number
/// (no suffix) is seconds, matching this domain's replication-lag semantics
/// (same convention `pg` uses); `10s`/`500ms`/`2m` are also accepted now
/// instead of only a bare integer or integer+`s`.
fn parse_lag_seconds(raw: &str) -> Result<i64> {
    let threshold =
        health::parse_threshold(raw).with_context(|| format!("invalid lag threshold {raw:?}"))?;
    Ok(threshold.as_seconds().round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lag_seconds_accepts_bare_and_suffixed_forms() {
        assert_eq!(parse_lag_seconds("10").unwrap(), 10);
        assert_eq!(parse_lag_seconds("10s").unwrap(), 10);
    }

    #[test]
    fn parse_lag_seconds_rejects_garbage() {
        assert!(parse_lag_seconds("abc").is_err());
        assert!(parse_lag_seconds("").is_err());
    }

    #[test]
    fn parse_lag_seconds_accepts_zero() {
        assert_eq!(parse_lag_seconds("0").unwrap(), 0);
        assert_eq!(parse_lag_seconds("0s").unwrap(), 0);
    }

    #[test]
    fn parse_lag_seconds_accepts_duration_suffixes() {
        assert_eq!(parse_lag_seconds("500ms").unwrap(), 1); // rounds to nearest second
        assert_eq!(parse_lag_seconds("2m").unwrap(), 120);
    }

    #[test]
    fn parse_args_surfaces_a_bad_warning_as_an_error() {
        let args = HealthArgs {
            warning: Some("not-a-number".to_string()),
            critical: None,
        };
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_args_accepts_absent_thresholds() {
        let args = HealthArgs {
            warning: None,
            critical: None,
        };
        assert_eq!(parse_args(&args).unwrap(), (None, None));
    }
}
