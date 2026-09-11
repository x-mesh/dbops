//! `dbops tcp check`: raw TCP connect check (PRD R32).

use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::TcpStream;

use crate::frame::ctx::parse_timeout;
use crate::frame::output::render_check;
use crate::frame::result::{CheckResult, CheckStatus, Metric};
use crate::frame::{exit, Ctx, ExitCode};

pub async fn check(
    ctx: &Ctx,
    address: &str,
    warning: Option<&str>,
    critical: Option<&str>,
) -> Result<ExitCode> {
    let warning_dur = warning
        .map(parse_timeout)
        .transpose()
        .context("--warning")?;
    let critical_dur = critical
        .map(parse_timeout)
        .transpose()
        .context("--critical")?;

    let started = Instant::now();
    let connect_result = tokio::time::timeout(ctx.timeout, TcpStream::connect(address)).await;
    let elapsed = started.elapsed();

    // Same UNKNOWN-vs-CRITICAL split as `http check`: a timeout means
    // "couldn't tell" (nothing answered within ctx.timeout), a refused or
    // otherwise failed connect means "definitely down".
    let result = match connect_result {
        Ok(Ok(_stream)) => {
            let mut status = CheckStatus::Ok;
            let mut reasons: Vec<String> = Vec::new();

            if let Some(crit) = critical_dur {
                if elapsed >= crit {
                    status = escalate(status, CheckStatus::Critical);
                    reasons.push(format!(
                        "connect time {}ms >= critical {}ms",
                        elapsed.as_millis(),
                        crit.as_millis()
                    ));
                }
            }
            if let Some(warn) = warning_dur {
                if elapsed >= warn {
                    status = escalate(status, CheckStatus::Warning);
                    reasons.push(format!(
                        "connect time {}ms >= warning {}ms",
                        elapsed.as_millis(),
                        warn.as_millis()
                    ));
                }
            }

            let summary = if reasons.is_empty() {
                format!("connected to {address} in {}ms", elapsed.as_millis())
            } else {
                format!(
                    "connected to {address} in {}ms; {}",
                    elapsed.as_millis(),
                    reasons.join(", ")
                )
            };

            CheckResult {
                status,
                summary,
                metrics: vec![Metric {
                    name: "connect_time".to_string(),
                    value: elapsed.as_secs_f64() * 1000.0,
                    unit: Some("ms".to_string()),
                    warn: warning_dur.map(|d| (d.as_secs_f64() * 1000.0).to_string()),
                    crit: critical_dur.map(|d| (d.as_secs_f64() * 1000.0).to_string()),
                }],
            }
        }
        Ok(Err(err)) => CheckResult {
            status: CheckStatus::Critical,
            summary: format!("failed to connect to {address}: {err}"),
            metrics: vec![],
        },
        Err(_timed_out) => CheckResult {
            status: CheckStatus::Unknown,
            summary: format!("timed out after {:?} connecting to {address}", ctx.timeout),
            metrics: vec![],
        },
    };

    println!("{}", render_check("tcp", "check", &result, ctx.json));
    Ok(ExitCode::from(exit::from_status(result.status)))
}

/// See `net::http::escalate` for why this exists instead of `Ord` on
/// `CheckStatus`. Duplicated rather than shared across `net`/`sys` so each
/// domain module stays self-contained per the L3 interface contract.
fn escalate(current: CheckStatus, candidate: CheckStatus) -> CheckStatus {
    fn rank(s: CheckStatus) -> u8 {
        match s {
            CheckStatus::Ok => 0,
            CheckStatus::Warning => 1,
            CheckStatus::Critical => 2,
            CheckStatus::Unknown => 3,
        }
    }
    if rank(candidate) > rank(current) {
        candidate
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalate_never_downgrades() {
        assert_eq!(
            escalate(CheckStatus::Critical, CheckStatus::Warning),
            CheckStatus::Critical
        );
        assert_eq!(
            escalate(CheckStatus::Ok, CheckStatus::Warning),
            CheckStatus::Warning
        );
    }
}
