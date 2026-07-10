//! `dbops redis health`: PING round-trip time against `--warning`/`--critical`
//! thresholds (milliseconds).

use std::time::Instant;

use anyhow::Result;

use crate::frame::result::{CheckResult, CheckStatus, Metric};
use crate::frame::{exit, output, Ctx, ExitCode, HealthArgs};
use crate::redis::connect;

/// Connection failures and command timeouts both render as `Unknown`
/// (exit 3) rather than `Critical` -- an unreachable server is a monitoring
/// blind spot, not a confirmed-bad state.
pub async fn run(args: &HealthArgs, ctx: &Ctx) -> Result<ExitCode> {
    let result = check(args, ctx).await;
    let code = exit::from_status(result.status);
    println!(
        "{}",
        output::render_check("redis", "health", &result, ctx.json)
    );
    Ok(ExitCode::from(code))
}

async fn check(args: &HealthArgs, ctx: &Ctx) -> CheckResult {
    let mut conn = match connect::connect(ctx).await {
        Ok(conn) => conn,
        Err(err) => return unknown(format!("connection failed: {err:#}")),
    };

    let start = Instant::now();
    let ping = connect::call::<String>(ctx, redis::cmd("PING").query_async(&mut conn)).await;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    match ping {
        Err(err) => unknown(format!("PING failed: {err:#}")),
        Ok(_) => {
            let status = evaluate_thresholds(elapsed_ms, args);
            CheckResult {
                status,
                summary: format!("PING responded in {elapsed_ms:.2}ms"),
                metrics: vec![Metric {
                    name: "response_time_ms".to_string(),
                    value: elapsed_ms,
                    unit: Some("ms".to_string()),
                    warn: args.warning.clone(),
                    crit: args.critical.clone(),
                }],
            }
        }
    }
}

fn unknown(summary: String) -> CheckResult {
    CheckResult {
        status: CheckStatus::Unknown,
        summary,
        metrics: vec![],
    }
}

/// Pure: response time (ms) + configured thresholds -> nagios status.
/// `>=` at each boundary, matching the check_postgres/nagios convention.
fn evaluate_thresholds(elapsed_ms: f64, args: &HealthArgs) -> CheckStatus {
    let parse = |raw: &Option<String>| raw.as_deref().and_then(|v| v.parse::<f64>().ok());
    if let Some(crit) = parse(&args.critical) {
        if elapsed_ms >= crit {
            return CheckStatus::Critical;
        }
    }
    if let Some(warn) = parse(&args.warning) {
        if elapsed_ms >= warn {
            return CheckStatus::Warning;
        }
    }
    CheckStatus::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(warning: Option<&str>, critical: Option<&str>) -> HealthArgs {
        HealthArgs {
            warning: warning.map(str::to_string),
            critical: critical.map(str::to_string),
        }
    }

    #[test]
    fn ok_when_below_every_threshold() {
        assert_eq!(
            evaluate_thresholds(5.0, &args(Some("50"), Some("100"))),
            CheckStatus::Ok
        );
    }

    #[test]
    fn warning_at_or_above_warning_threshold() {
        assert_eq!(
            evaluate_thresholds(50.0, &args(Some("50"), Some("100"))),
            CheckStatus::Warning
        );
    }

    #[test]
    fn critical_at_or_above_critical_threshold() {
        assert_eq!(
            evaluate_thresholds(150.0, &args(Some("50"), Some("100"))),
            CheckStatus::Critical
        );
    }

    #[test]
    fn critical_wins_when_both_thresholds_are_crossed() {
        assert_eq!(
            evaluate_thresholds(200.0, &args(Some("50"), Some("100"))),
            CheckStatus::Critical
        );
    }

    #[test]
    fn ok_when_no_thresholds_configured() {
        assert_eq!(
            evaluate_thresholds(99999.0, &args(None, None)),
            CheckStatus::Ok
        );
    }
}
