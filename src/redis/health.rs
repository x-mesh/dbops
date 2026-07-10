//! `dbops redis health`: PING round-trip time against `--warning`/`--critical`
//! thresholds (milliseconds).

use std::time::Instant;

use anyhow::{Context, Result};

use crate::frame::result::{CheckResult, CheckStatus, Metric};
use crate::frame::{exit, health, output, Ctx, ExitCode, HealthArgs};
use crate::redis::connect;

/// Connection failures and command timeouts both render as `Unknown`
/// (exit 3) rather than `Critical` -- an unreachable server is a monitoring
/// blind spot, not a confirmed-bad state.
pub async fn run(args: &HealthArgs, ctx: &Ctx) -> Result<ExitCode> {
    // A bad --warning/--critical value is a usage error, not a connectivity
    // problem: reject it here, directly, with a plain stderr message and
    // exit 3 -- previously this got silently treated as "no threshold
    // configured" (`.parse::<f64>().ok()` swallowed the error), so a typo'd
    // flag disabled the check without any indication anything was wrong.
    let (warning, critical) = match parse_args(args) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(exit::unix::ARGUMENT_ERROR));
        }
    };

    let result = check(ctx, warning, critical).await;
    let code = exit::from_status(result.status);
    println!(
        "{}",
        output::render_check("redis", "health", &result, ctx.json)
    );
    Ok(ExitCode::from(code))
}

fn parse_args(args: &HealthArgs) -> Result<(Option<f64>, Option<f64>)> {
    let warning = args
        .warning
        .as_deref()
        .map(parse_response_ms)
        .transpose()
        .context("invalid --warning value")?;
    let critical = args
        .critical
        .as_deref()
        .map(parse_response_ms)
        .transpose()
        .context("invalid --critical value")?;
    Ok((warning, critical))
}

/// Parse a `--warning`/`--critical` response-time threshold via the shared
/// duration/count parser (`0` allowed -- "alert on any response time at
/// all"). A bare number (no suffix) is milliseconds, matching this domain's
/// legacy convention (`--warning 50` == 50ms); `500ms`/`5s`/etc. now also
/// work instead of silently being dropped.
fn parse_response_ms(raw: &str) -> Result<f64> {
    let threshold = health::parse_threshold(raw)
        .with_context(|| format!("invalid response-time threshold {raw:?}"))?;
    Ok(threshold.as_millis_f64())
}

async fn check(ctx: &Ctx, warning: Option<f64>, critical: Option<f64>) -> CheckResult {
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
            let status = evaluate_thresholds(elapsed_ms, warning, critical);
            CheckResult {
                status,
                summary: format!("PING responded in {elapsed_ms:.2}ms"),
                metrics: vec![Metric {
                    name: "response_time_ms".to_string(),
                    value: elapsed_ms,
                    unit: Some("ms".to_string()),
                    warn: warning.map(|w| w.to_string()),
                    crit: critical.map(|c| c.to_string()),
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
fn evaluate_thresholds(
    elapsed_ms: f64,
    warning: Option<f64>,
    critical: Option<f64>,
) -> CheckStatus {
    if let Some(crit) = critical {
        if elapsed_ms >= crit {
            return CheckStatus::Critical;
        }
    }
    if let Some(warn) = warning {
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
            evaluate_thresholds(5.0, Some(50.0), Some(100.0)),
            CheckStatus::Ok
        );
    }

    #[test]
    fn warning_at_or_above_warning_threshold() {
        assert_eq!(
            evaluate_thresholds(50.0, Some(50.0), Some(100.0)),
            CheckStatus::Warning
        );
    }

    #[test]
    fn critical_at_or_above_critical_threshold() {
        assert_eq!(
            evaluate_thresholds(150.0, Some(50.0), Some(100.0)),
            CheckStatus::Critical
        );
    }

    #[test]
    fn critical_wins_when_both_thresholds_are_crossed() {
        assert_eq!(
            evaluate_thresholds(200.0, Some(50.0), Some(100.0)),
            CheckStatus::Critical
        );
    }

    #[test]
    fn ok_when_no_thresholds_configured() {
        assert_eq!(evaluate_thresholds(99999.0, None, None), CheckStatus::Ok);
    }

    #[test]
    fn parse_args_accepts_zero() {
        let (_, critical) = parse_args(&args(None, Some("0"))).unwrap();
        assert_eq!(critical, Some(0.0));
    }

    #[test]
    fn parse_args_accepts_duration_suffixes_as_milliseconds() {
        // Previously "0ms"/"5s" failed plain f64::parse and were silently
        // dropped as "no threshold configured" -- now they resolve to the
        // equivalent millisecond value instead of erroring or vanishing.
        let (_, critical) = parse_args(&args(None, Some("0ms"))).unwrap();
        assert_eq!(critical, Some(0.0));

        let (warning, _) = parse_args(&args(Some("5s"), None)).unwrap();
        assert_eq!(warning, Some(5000.0));
    }

    #[test]
    fn parse_args_rejects_garbage_instead_of_silently_disabling_the_check() {
        assert!(parse_args(&args(None, Some("not-a-number"))).is_err());
    }
}
