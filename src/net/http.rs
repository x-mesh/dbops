//! `dbops http check`: HTTP(S) endpoint check (PRD R31).
//!
//! Only the response status and headers are inspected; the body is never
//! read (no `.text()`/`.bytes()` call), so there's nothing that needs an
//! explicit byte-size cap: reqwest doesn't download a response body until
//! something asks for it, and we never ask.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::redirect::Policy;

use crate::frame::ctx::parse_timeout;
use crate::frame::output::render_check;
use crate::frame::result::{CheckResult, CheckStatus, Metric};
use crate::frame::{exit, Ctx, ExitCode};

use super::cert;

/// Redirect ceiling (discretionary; PRD R31 doesn't pin an exact number).
const MAX_REDIRECTS: usize = 10;

/// TLS certificate expiry thresholds (discretionary, per task brief: 30
/// days => WARNING, 7 days => CRITICAL).
const CERT_WARNING_DAYS: i64 = 30;
const CERT_CRITICAL_DAYS: i64 = 7;

pub async fn check(
    ctx: &Ctx,
    url: &str,
    expect_status: Option<u16>,
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

    let mut builder = reqwest::Client::builder()
        .timeout(ctx.timeout)
        .redirect(Policy::limited(MAX_REDIRECTS))
        .tls_info(true);
    // Two separate paths, not `danger_accept_invalid_certs(ctx.insecure)`:
    // that flag is only honored on reqwest's own TLS config, whose default
    // verifier reads the system trust store and fails to build on an image
    // with no CA bundle. The secure path swaps in compiled-in roots (see
    // frame::tls) so verification works there; `--insecure` keeps reqwest's
    // no-verify path, which touches no trust store and already builds fine.
    builder = if ctx.insecure {
        builder.danger_accept_invalid_certs(true)
    } else {
        builder.use_preconfigured_tls(crate::frame::tls::bundled_root_config()?)
    };
    let client = builder.build().context("build http client")?;

    let started = Instant::now();
    let send_result = client.get(url).send().await;
    let elapsed = started.elapsed();

    // Timeout vs. everything-else-that-failed get different statuses: a
    // timeout means "couldn't tell" (UNKNOWN), a refused/unreachable
    // connection means "definitely down" (CRITICAL).
    let result = match send_result {
        Ok(resp) => evaluate_response(resp, expect_status, elapsed, warning_dur, critical_dur),
        Err(err) if err.is_timeout() => CheckResult {
            status: CheckStatus::Unknown,
            summary: format!("timed out after {:?} requesting {url}", ctx.timeout),
            metrics: vec![],
        },
        Err(err) => CheckResult {
            status: CheckStatus::Critical,
            summary: format!("request to {url} failed: {err}"),
            metrics: vec![],
        },
    };

    println!("{}", render_check("http", "check", &result, ctx.json));
    Ok(ExitCode::from(exit::from_status(result.status)))
}

fn evaluate_response(
    resp: reqwest::Response,
    expect_status: Option<u16>,
    elapsed: Duration,
    warning: Option<Duration>,
    critical: Option<Duration>,
) -> CheckResult {
    let status_code = resp.status();
    let is_https = resp.url().scheme() == "https";
    let tls_info = resp.extensions().get::<reqwest::tls::TlsInfo>();

    let mut metrics = vec![Metric {
        name: "response_time".to_string(),
        value: elapsed.as_secs_f64() * 1000.0,
        unit: Some("ms".to_string()),
        warn: warning.map(|d| (d.as_secs_f64() * 1000.0).to_string()),
        crit: critical.map(|d| (d.as_secs_f64() * 1000.0).to_string()),
    }];

    let (mut status, mut reasons) = classify_status(status_code, expect_status);

    if let Some(crit) = critical {
        if elapsed >= crit {
            status = escalate(status, CheckStatus::Critical);
            reasons.push(format!(
                "response time {}ms >= critical {}ms",
                elapsed.as_millis(),
                crit.as_millis()
            ));
        }
    }
    if let Some(warn) = warning {
        if elapsed >= warn {
            status = escalate(status, CheckStatus::Warning);
            reasons.push(format!(
                "response time {}ms >= warning {}ms",
                elapsed.as_millis(),
                warn.as_millis()
            ));
        }
    }

    if is_https {
        match tls_info
            .and_then(|info| info.peer_certificate())
            .and_then(cert::parse_not_after)
        {
            Some(not_after_unix) => {
                let days_left = cert::days_until(not_after_unix);
                metrics.push(Metric {
                    name: "cert_expiry".to_string(),
                    value: days_left as f64,
                    unit: Some("days".to_string()),
                    warn: Some(CERT_WARNING_DAYS.to_string()),
                    crit: Some(CERT_CRITICAL_DAYS.to_string()),
                });
                if days_left <= CERT_CRITICAL_DAYS {
                    status = escalate(status, CheckStatus::Critical);
                    reasons.push(format!("TLS cert expires in {days_left}d"));
                } else if days_left <= CERT_WARNING_DAYS {
                    status = escalate(status, CheckStatus::Warning);
                    reasons.push(format!("TLS cert expires in {days_left}d"));
                }
            }
            None => {
                reasons.push("TLS cert expiry: unavailable (parse failed)".to_string());
            }
        }
    }

    let summary = if reasons.is_empty() {
        format!("{status_code} in {}ms", elapsed.as_millis())
    } else {
        format!(
            "{status_code} in {}ms; {}",
            elapsed.as_millis(),
            reasons.join(", ")
        )
    };

    CheckResult {
        status,
        summary,
        metrics,
    }
}

/// Status-code -> check-status mapping (discretionary): an explicit
/// `--expect-status` mismatch is always CRITICAL, otherwise 5xx is
/// CRITICAL and 4xx is WARNING (a 404 on a monitored endpoint is worth a
/// heads-up, but it isn't "the service is down").
fn classify_status(status: reqwest::StatusCode, expect: Option<u16>) -> (CheckStatus, Vec<String>) {
    if let Some(expect) = expect {
        if status.as_u16() != expect {
            return (
                CheckStatus::Critical,
                vec![format!("expected status {expect}, got {status}")],
            );
        }
        return (CheckStatus::Ok, vec![]);
    }
    if status.is_server_error() {
        (
            CheckStatus::Critical,
            vec![format!("server error {status}")],
        )
    } else if status.is_client_error() {
        (CheckStatus::Warning, vec![format!("client error {status}")])
    } else {
        (CheckStatus::Ok, vec![])
    }
}

/// Raises `current` to `candidate` only if `candidate` is more severe.
/// `CheckStatus` has no `Ord` impl (frame/result.rs is out of scope for
/// this task), so this is a small local severity order instead, distinct
/// from `frame::exit::from_status`'s nagios *exit code* numbering, which
/// isn't a severity ranking.
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
    fn classify_status_flags_expect_status_mismatch_as_critical() {
        let (status, reasons) = classify_status(reqwest::StatusCode::OK, Some(201));
        assert_eq!(status, CheckStatus::Critical);
        assert!(reasons[0].contains("expected status 201"));
    }

    #[test]
    fn classify_status_treats_5xx_as_critical_and_4xx_as_warning() {
        assert_eq!(
            classify_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR, None).0,
            CheckStatus::Critical
        );
        assert_eq!(
            classify_status(reqwest::StatusCode::NOT_FOUND, None).0,
            CheckStatus::Warning
        );
        assert_eq!(
            classify_status(reqwest::StatusCode::OK, None).0,
            CheckStatus::Ok
        );
    }

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
        assert_eq!(
            escalate(CheckStatus::Warning, CheckStatus::Ok),
            CheckStatus::Warning
        );
    }
}
