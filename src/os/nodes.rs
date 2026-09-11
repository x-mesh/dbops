//! `dbops os nodes`: combines `GET _cat/nodes` (node identity: name, role,
//! elected cluster-manager) with `GET _nodes/stats/fs,jvm` (byte-exact disk
//! and heap usage) into one [`StatReport`].
//!
//! Disk usage is compared against the well-known OpenSearch/Elasticsearch
//! default watermark percentages (`cluster.routing.allocation.disk.watermark.*`:
//! low 85%, high 90%, flood-stage 95%). These are **not** read from
//! `_cluster/settings`. A cluster running with custom watermarks will be
//! compared against the defaults, not its actual configured values. Known
//! limitation, documented here rather than silently wrong.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use opensearch::nodes::NodesStatsParts;
use opensearch::OpenSearch;
use serde_json::Value;

use crate::frame::result::StatReport;

const WATERMARK_LOW_PCT: f64 = 85.0;
const WATERMARK_HIGH_PCT: f64 = 90.0;
const WATERMARK_FLOOD_PCT: f64 = 95.0;

struct RoleInfo {
    role: String,
    master: bool,
}

struct NodeStat {
    disk_used_pct: Option<f64>,
    heap_used_pct: Option<f64>,
}

pub async fn nodes(client: &OpenSearch, timeout: Duration) -> Result<StatReport> {
    let roles = fetch_roles(client, timeout).await?;
    let stats = fetch_stats(client, timeout).await?;

    let mut names: Vec<&String> = stats.keys().collect();
    names.sort();

    let columns = vec![
        "node".to_string(),
        "role".to_string(),
        "manager".to_string(),
        "disk_used_pct".to_string(),
        "disk_watermark".to_string(),
        "heap_used_pct".to_string(),
    ];

    let rows = names
        .into_iter()
        .map(|name| {
            let stat = &stats[name];
            let role_info = roles.get(name);
            vec![
                name.clone(),
                role_info.map_or_else(|| "?".to_string(), |r| r.role.clone()),
                role_info.map_or_else(
                    || "?".to_string(),
                    |r| if r.master { "*" } else { "-" }.to_string(),
                ),
                stat.disk_used_pct
                    .map_or_else(|| "?".to_string(), |v| format!("{v:.1}%")),
                stat.disk_used_pct
                    .map_or_else(|| "-".to_string(), watermark_label),
                stat.heap_used_pct
                    .map_or_else(|| "?".to_string(), |v| format!("{v:.1}%")),
            ]
        })
        .collect();

    Ok(StatReport { columns, rows })
}

fn watermark_label(disk_used_pct: f64) -> String {
    if disk_used_pct >= WATERMARK_FLOOD_PCT {
        format!("flood-stage (>={WATERMARK_FLOOD_PCT:.0}%)")
    } else if disk_used_pct >= WATERMARK_HIGH_PCT {
        format!("high (>={WATERMARK_HIGH_PCT:.0}%)")
    } else if disk_used_pct >= WATERMARK_LOW_PCT {
        format!("low (>={WATERMARK_LOW_PCT:.0}%)")
    } else {
        "-".to_string()
    }
}

async fn fetch_roles(client: &OpenSearch, timeout: Duration) -> Result<BTreeMap<String, RoleInfo>> {
    let cat = client.cat();
    let fut = cat
        .nodes()
        .format("json")
        .h(&["name", "node.role", "master"])
        .send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("_cat/nodes timed out")?
        .context("failed to query _cat/nodes")?;
    let body: Vec<Value> = response
        .json()
        .await
        .context("failed to parse _cat/nodes response")?;

    let mut out = BTreeMap::new();
    for entry in body {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let role = entry
            .get("node.role")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let master = entry.get("master").and_then(Value::as_str) == Some("*");
        out.insert(name, RoleInfo { role, master });
    }
    Ok(out)
}

async fn fetch_stats(client: &OpenSearch, timeout: Duration) -> Result<BTreeMap<String, NodeStat>> {
    let nodes_ns = client.nodes();
    let fut = nodes_ns
        .stats(NodesStatsParts::Metric(&["fs", "jvm"]))
        .send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("_nodes/stats timed out")?
        .context("failed to query _nodes/stats")?;
    let body: Value = response
        .json()
        .await
        .context("failed to parse _nodes/stats response")?;

    let nodes_obj = body
        .get("nodes")
        .and_then(Value::as_object)
        .context("_nodes/stats response missing 'nodes'")?;

    let mut out = BTreeMap::new();
    for node in nodes_obj.values() {
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let disk_used_pct = node
            .pointer("/fs/total/total_in_bytes")
            .and_then(Value::as_f64)
            .zip(
                node.pointer("/fs/total/available_in_bytes")
                    .and_then(Value::as_f64),
            )
            .filter(|(total, _)| *total > 0.0)
            .map(|(total, avail)| (total - avail) / total * 100.0);
        let heap_used_pct = node
            .pointer("/jvm/mem/heap_used_percent")
            .and_then(Value::as_f64);
        out.insert(
            name,
            NodeStat {
                disk_used_pct,
                heap_used_pct,
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_label_below_low_is_dash() {
        assert_eq!(watermark_label(50.0), "-");
        assert_eq!(watermark_label(84.9), "-");
    }

    #[test]
    fn watermark_label_at_low_boundary() {
        assert_eq!(watermark_label(85.0), "low (>=85%)");
        assert_eq!(watermark_label(89.9), "low (>=85%)");
    }

    #[test]
    fn watermark_label_at_high_boundary() {
        assert_eq!(watermark_label(90.0), "high (>=90%)");
        assert_eq!(watermark_label(94.9), "high (>=90%)");
    }

    #[test]
    fn watermark_label_at_flood_boundary() {
        assert_eq!(watermark_label(95.0), "flood-stage (>=95%)");
        assert_eq!(watermark_label(100.0), "flood-stage (>=95%)");
    }
}
