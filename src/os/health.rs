//! `dbops os health` — wraps `GET _cluster/health`.
//!
//! Status mapping (PRD): `green` -> OK, `yellow` -> WARNING, `red` ->
//! CRITICAL. Any transport failure or a response that doesn't arrive within
//! `timeout` maps to UNKNOWN, matching standard nagios-plugin semantics
//! (`from_status(Unknown) == 3`, checked in `frame::exit`).
//!
//! `--warning`/`--critical` design decision: the PRD leaves the exact metric
//! up to this task. This check applies both thresholds to `unassigned_shards`
//! — of the five metrics OpenSearch reports, it's the most direct signal of
//! an actual data-availability problem (unlike `pending_tasks`, which is
//! often just transient cluster-manager churn). A threshold breach only ever
//! *escalates* the color-derived status (via [`worse`]), never downgrades a
//! `red` cluster to `WARNING` just because `unassigned_shards` happens to be
//! under threshold.

use std::time::Duration;

use opensearch::cluster::ClusterHealthParts;
use opensearch::OpenSearch;
use serde::Deserialize;

use crate::frame::result::{CheckResult, CheckStatus, Metric};
use crate::frame::HealthArgs;

#[derive(Debug, Deserialize)]
struct ClusterHealthBody {
    status: String,
    unassigned_shards: u64,
    initializing_shards: u64,
    relocating_shards: u64,
    number_of_pending_tasks: u64,
    active_shards_percent_as_number: f64,
}

pub async fn health(client: &OpenSearch, timeout: Duration, args: &HealthArgs) -> CheckResult {
    let warning = match parse_threshold("--warning", args.warning.as_deref()) {
        Ok(v) => v,
        Err(msg) => return unknown(msg),
    };
    let critical = match parse_threshold("--critical", args.critical.as_deref()) {
        Ok(v) => v,
        Err(msg) => return unknown(msg),
    };

    let cluster = client.cluster();
    let fetch = cluster.health(ClusterHealthParts::None).send();
    match tokio::time::timeout(timeout, fetch).await {
        Ok(Ok(response)) => {
            let status_code = response.status_code();
            if !status_code.is_success() {
                let body_text = response.text().await.unwrap_or_default();
                return unknown(format!(
                    "opensearch returned HTTP {status_code}: {body_text}"
                ));
            }
            match response.json::<ClusterHealthBody>().await {
                Ok(body) => build_result(&body, warning, critical),
                Err(err) => unknown(format!(
                    "failed to parse opensearch cluster health response: {err}"
                )),
            }
        }
        Ok(Err(err)) => unknown(format!("failed to reach opensearch cluster: {err}")),
        Err(_) => unknown(format!(
            "opensearch cluster health check timed out after {:.1}s",
            timeout.as_secs_f64()
        )),
    }
}

fn parse_threshold(flag: &str, raw: Option<&str>) -> Result<Option<u64>, String> {
    match raw {
        None => Ok(None),
        Some(v) => v
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("invalid {flag} value {v:?}: expected a non-negative integer")),
    }
}

fn build_result(
    body: &ClusterHealthBody,
    warning: Option<u64>,
    critical: Option<u64>,
) -> CheckResult {
    let base_status = match body.status.as_str() {
        "green" => CheckStatus::Ok,
        "yellow" => CheckStatus::Warning,
        "red" => CheckStatus::Critical,
        other => {
            return unknown(format!(
                "opensearch reported an unrecognized cluster status: {other:?}"
            ))
        }
    };

    let threshold_status = match critical {
        Some(c) if body.unassigned_shards >= c => Some(CheckStatus::Critical),
        _ => match warning {
            Some(w) if body.unassigned_shards >= w => Some(CheckStatus::Warning),
            _ => None,
        },
    };
    let status = threshold_status.map_or(base_status, |t| worse(base_status, t));

    let summary = format!(
        "cluster status {} ({} unassigned shard{}, {:.1}% active shards)",
        body.status,
        body.unassigned_shards,
        if body.unassigned_shards == 1 { "" } else { "s" },
        body.active_shards_percent_as_number,
    );

    let metrics = vec![
        Metric {
            name: "unassigned_shards".to_string(),
            value: body.unassigned_shards as f64,
            unit: None,
            warn: warning.map(|w| w.to_string()),
            crit: critical.map(|c| c.to_string()),
        },
        Metric {
            name: "initializing_shards".to_string(),
            value: body.initializing_shards as f64,
            unit: None,
            warn: None,
            crit: None,
        },
        Metric {
            name: "relocating_shards".to_string(),
            value: body.relocating_shards as f64,
            unit: None,
            warn: None,
            crit: None,
        },
        Metric {
            name: "pending_tasks".to_string(),
            value: body.number_of_pending_tasks as f64,
            unit: None,
            warn: None,
            crit: None,
        },
        Metric {
            name: "active_shards_percent".to_string(),
            value: body.active_shards_percent_as_number,
            unit: Some("%".to_string()),
            warn: None,
            crit: None,
        },
    ];

    CheckResult {
        status,
        summary,
        metrics,
    }
}

/// The worse (higher-severity) of two statuses, `Ok < Warning < Critical`.
/// Only ever called with color-derived statuses on both sides — `Unknown`
/// is produced solely by early-return paths in [`health`] that never reach
/// here, so it has no defined rank in this ordering.
fn worse(a: CheckStatus, b: CheckStatus) -> CheckStatus {
    fn rank(s: CheckStatus) -> u8 {
        match s {
            CheckStatus::Ok => 0,
            CheckStatus::Warning => 1,
            CheckStatus::Critical => 2,
            CheckStatus::Unknown => 3,
        }
    }
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

fn unknown(summary: String) -> CheckResult {
    CheckResult {
        status: CheckStatus::Unknown,
        summary,
        metrics: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(status: &str, unassigned: u64) -> ClusterHealthBody {
        ClusterHealthBody {
            status: status.to_string(),
            unassigned_shards: unassigned,
            initializing_shards: 0,
            relocating_shards: 0,
            number_of_pending_tasks: 0,
            active_shards_percent_as_number: 100.0,
        }
    }

    #[test]
    fn green_maps_to_ok() {
        let result = build_result(&body("green", 0), None, None);
        assert_eq!(result.status, CheckStatus::Ok);
    }

    #[test]
    fn yellow_maps_to_warning() {
        let result = build_result(&body("yellow", 0), None, None);
        assert_eq!(result.status, CheckStatus::Warning);
    }

    #[test]
    fn red_maps_to_critical() {
        let result = build_result(&body("red", 0), None, None);
        assert_eq!(result.status, CheckStatus::Critical);
    }

    #[test]
    fn unrecognized_status_maps_to_unknown() {
        let result = build_result(&body("mauve", 0), None, None);
        assert_eq!(result.status, CheckStatus::Unknown);
        assert!(result.summary.contains("mauve"));
    }

    #[test]
    fn warning_threshold_escalates_green_to_warning() {
        let result = build_result(&body("green", 5), Some(3), None);
        assert_eq!(result.status, CheckStatus::Warning);
    }

    #[test]
    fn critical_threshold_escalates_green_to_critical() {
        let result = build_result(&body("green", 10), Some(3), Some(8));
        assert_eq!(result.status, CheckStatus::Critical);
    }

    #[test]
    fn threshold_never_downgrades_red() {
        // unassigned_shards is under both thresholds, but the cluster color
        // is red -- must stay CRITICAL, never get pulled down to WARNING.
        let result = build_result(&body("red", 1), Some(100), Some(200));
        assert_eq!(result.status, CheckStatus::Critical);
    }

    #[test]
    fn threshold_below_warning_leaves_status_untouched() {
        let result = build_result(&body("green", 1), Some(5), Some(10));
        assert_eq!(result.status, CheckStatus::Ok);
    }

    #[test]
    fn unassigned_shards_metric_carries_threshold_strings() {
        let result = build_result(&body("green", 2), Some(5), Some(10));
        let metric = result
            .metrics
            .iter()
            .find(|m| m.name == "unassigned_shards")
            .unwrap();
        assert_eq!(metric.warn.as_deref(), Some("5"));
        assert_eq!(metric.crit.as_deref(), Some("10"));
    }

    #[test]
    fn metrics_cover_every_prd_field() {
        let result = build_result(&body("green", 0), None, None);
        let names: Vec<&str> = result.metrics.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "unassigned_shards",
                "initializing_shards",
                "relocating_shards",
                "pending_tasks",
                "active_shards_percent",
            ]
        );
    }

    #[test]
    fn parse_threshold_accepts_valid_integers() {
        assert_eq!(parse_threshold("--warning", Some("10")).unwrap(), Some(10));
        assert_eq!(parse_threshold("--warning", None).unwrap(), None);
    }

    #[test]
    fn parse_threshold_rejects_garbage() {
        let err = parse_threshold("--warning", Some("abc")).unwrap_err();
        assert!(err.contains("--warning"));
        assert!(err.contains("abc"));
    }

    #[test]
    fn parse_threshold_rejects_negative_numbers() {
        assert!(parse_threshold("--critical", Some("-1")).is_err());
    }

    #[test]
    fn worse_picks_the_higher_severity() {
        assert_eq!(
            worse(CheckStatus::Ok, CheckStatus::Warning),
            CheckStatus::Warning
        );
        assert_eq!(
            worse(CheckStatus::Critical, CheckStatus::Warning),
            CheckStatus::Critical
        );
        assert_eq!(worse(CheckStatus::Ok, CheckStatus::Ok), CheckStatus::Ok);
    }
}
