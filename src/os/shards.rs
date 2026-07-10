//! `dbops os shards` — wraps `GET _cat/shards?format=json`.
//!
//! Renders two separate tables rather than one: a shard-granularity list of
//! everything currently `UNASSIGNED` (the actionable signal — data at risk
//! or actively recovering) and a node-granularity shard-count distribution
//! (the imbalance signal — one node quietly holding far more shards than
//! its peers). The two have different row shapes (per-shard vs per-node),
//! so folding them into one table would force one of them into columns
//! that don't apply to it. `mod::run` prints the unassigned table first,
//! per the PRD's "우선 표시".

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use opensearch::cat::CatShardsParts;
use opensearch::OpenSearch;
use serde_json::Value;

use crate::frame::result::StatReport;

const CAT_SHARDS_FIELDS: &[&str] = &["index", "shard", "prirep", "state", "unassigned.reason", "node"];

pub struct ShardsReport {
    pub unassigned: StatReport,
    pub distribution: StatReport,
}

pub async fn shards(client: &OpenSearch, timeout: Duration) -> Result<ShardsReport> {
    let entries = fetch(client, timeout).await?;
    Ok(build_reports(&entries))
}

async fn fetch(client: &OpenSearch, timeout: Duration) -> Result<Vec<Value>> {
    let cat = client.cat();
    let fut = cat.shards(CatShardsParts::None).format("json").h(CAT_SHARDS_FIELDS).send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("_cat/shards timed out")?
        .context("failed to query _cat/shards")?;
    let body: Vec<Value> = response.json().await.context("failed to parse _cat/shards response")?;
    Ok(body)
}

/// Pure: raw `_cat/shards` JSON rows -> the unassigned-shard list and the
/// per-node shard-count distribution. A shard only contributes to the
/// distribution once it has a `node` (unassigned shards have none).
fn build_reports(entries: &[Value]) -> ShardsReport {
    let mut unassigned_rows: Vec<Vec<String>> = Vec::new();
    let mut node_counts: BTreeMap<String, u64> = BTreeMap::new();

    for entry in entries {
        let field = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or("-").to_string();
        let state = field("state");

        if state == "UNASSIGNED" {
            unassigned_rows.push(vec![field("index"), field("shard"), field("prirep"), state, field("unassigned.reason")]);
        }

        if let Some(node) = entry.get("node").and_then(Value::as_str).filter(|n| !n.is_empty()) {
            *node_counts.entry(node.to_string()).or_insert(0) += 1;
        }
    }

    unassigned_rows.sort();

    // Highest shard count first -- that's the node most likely to be the
    // imbalance the operator is hunting for.
    let mut distribution: Vec<(String, u64)> = node_counts.into_iter().collect();
    distribution.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    ShardsReport {
        unassigned: StatReport {
            columns: vec!["index", "shard", "prirep", "state", "unassigned.reason"]
                .into_iter()
                .map(String::from)
                .collect(),
            rows: unassigned_rows,
        },
        distribution: StatReport {
            columns: vec!["node", "shards"].into_iter().map(String::from).collect(),
            rows: distribution.into_iter().map(|(node, count)| vec![node, count.to_string()]).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assigned(index: &str, shard: &str, prirep: &str, node: &str) -> Value {
        json!({"index": index, "shard": shard, "prirep": prirep, "state": "STARTED", "node": node})
    }

    fn unassigned(index: &str, shard: &str, prirep: &str, reason: &str) -> Value {
        json!({"index": index, "shard": shard, "prirep": prirep, "state": "UNASSIGNED", "unassigned.reason": reason})
    }

    #[test]
    fn unassigned_shards_are_listed_with_reason() {
        let entries = vec![
            assigned("myindex", "0", "p", "node-1"),
            unassigned("myindex", "0", "r", "REPLICA_ADDED"),
        ];
        let report = build_reports(&entries);
        assert_eq!(report.unassigned.rows.len(), 1);
        assert_eq!(report.unassigned.rows[0], vec!["myindex", "0", "r", "UNASSIGNED", "REPLICA_ADDED"]);
    }

    #[test]
    fn no_unassigned_shards_yields_empty_table() {
        let entries = vec![assigned("myindex", "0", "p", "node-1")];
        let report = build_reports(&entries);
        assert!(report.unassigned.rows.is_empty());
    }

    #[test]
    fn distribution_counts_shards_per_node() {
        let entries = vec![
            assigned("a", "0", "p", "node-1"),
            assigned("a", "1", "p", "node-1"),
            assigned("b", "0", "p", "node-2"),
        ];
        let report = build_reports(&entries);
        assert_eq!(report.distribution.rows, vec![vec!["node-1", "2"], vec!["node-2", "1"]]);
    }

    #[test]
    fn distribution_ignores_unassigned_shards() {
        let entries = vec![assigned("a", "0", "p", "node-1"), unassigned("a", "1", "r", "NODE_LEFT")];
        let report = build_reports(&entries);
        assert_eq!(report.distribution.rows, vec![vec!["node-1", "1"]]);
    }

    #[test]
    fn distribution_sorts_by_count_descending_then_node_name() {
        let entries = vec![
            assigned("a", "0", "p", "node-b"),
            assigned("a", "1", "p", "node-a"),
            assigned("a", "2", "p", "node-a"),
        ];
        let report = build_reports(&entries);
        assert_eq!(report.distribution.rows, vec![vec!["node-a", "2"], vec!["node-b", "1"]]);
    }

    #[test]
    fn unassigned_rows_are_sorted() {
        let entries = vec![unassigned("zzz", "0", "r", "x"), unassigned("aaa", "0", "r", "x")];
        let report = build_reports(&entries);
        assert_eq!(report.unassigned.rows[0][0], "aaa");
        assert_eq!(report.unassigned.rows[1][0], "zzz");
    }
}
