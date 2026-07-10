//! `dbops os stats [--index <pattern>]` — wraps `GET <pattern|_all>/_stats`.
//!
//! `docs.count` is read from each index's `primaries` block (replicas would
//! otherwise double-count documents); `store.size`/`indexing.index_total`/
//! `search.query_total` are read from `total` (primaries + replicas), which
//! matches what `dbops os indices`' `store.size` column already reports.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use opensearch::indices::IndicesStatsParts;
use opensearch::OpenSearch;
use serde_json::Value;

use crate::frame::result::StatReport;
use crate::os::indices::human_bytes;

pub async fn stats(client: &OpenSearch, timeout: Duration, index: Option<&str>) -> Result<StatReport> {
    let body = fetch(client, timeout, index).await?;
    Ok(build_report(&body))
}

async fn fetch(client: &OpenSearch, timeout: Duration, index: Option<&str>) -> Result<Value> {
    let parts = match index {
        Some(pattern) => IndicesStatsParts::Index(&[pattern]),
        None => IndicesStatsParts::None,
    };
    let indices_ns = client.indices();
    let fut = indices_ns.stats(parts).send();
    let response = tokio::time::timeout(timeout, fut).await.context("_stats timed out")?.context("failed to query _stats")?;

    let status_code = response.status_code();
    if !status_code.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        bail!("opensearch returned HTTP {status_code}: {body_text}");
    }
    response.json().await.context("failed to parse _stats response")
}

/// Pure: raw `_stats` JSON body -> a per-index [`StatReport`], sorted by
/// index name. Missing fields render as `"-"` rather than being dropped, so
/// `--json` output always has the same shape regardless of server version.
fn build_report(body: &Value) -> StatReport {
    let mut rows: Vec<Vec<String>> = Vec::new();

    if let Some(indices) = body.get("indices").and_then(Value::as_object) {
        let mut names: Vec<&String> = indices.keys().collect();
        names.sort();

        for name in names {
            let entry = &indices[name];
            let docs_count = entry
                .pointer("/primaries/docs/count")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_string(), |v| v.to_string());
            let store_size = entry
                .pointer("/total/store/size_in_bytes")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_string(), human_bytes);
            let index_total = entry
                .pointer("/total/indexing/index_total")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_string(), |v| v.to_string());
            let query_total = entry
                .pointer("/total/search/query_total")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_string(), |v| v.to_string());

            rows.push(vec![name.clone(), docs_count, store_size, index_total, query_total]);
        }
    }

    StatReport {
        columns: vec!["index", "docs.count", "store.size", "indexing.index_total", "search.query_total"]
            .into_iter()
            .map(String::from)
            .collect(),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "indices": {
                "myindex": {
                    "primaries": {"docs": {"count": 100}},
                    "total": {
                        "store": {"size_in_bytes": 2_097_152u64},
                        "indexing": {"index_total": 250},
                        "search": {"query_total": 42},
                    }
                },
                "other": {
                    "primaries": {"docs": {"count": 5}},
                    "total": {
                        "store": {"size_in_bytes": 1024u64},
                        "indexing": {"index_total": 1},
                        "search": {"query_total": 0},
                    }
                },
            }
        })
    }

    #[test]
    fn builds_rows_for_every_index() {
        let report = build_report(&fixture());
        assert_eq!(report.rows.len(), 2);
    }

    #[test]
    fn rows_are_sorted_by_index_name() {
        let report = build_report(&fixture());
        assert_eq!(report.rows[0][0], "myindex");
        assert_eq!(report.rows[1][0], "other");
    }

    #[test]
    fn docs_count_comes_from_primaries() {
        let report = build_report(&fixture());
        let row = report.rows.iter().find(|r| r[0] == "myindex").unwrap();
        assert_eq!(row[1], "100");
    }

    #[test]
    fn store_size_is_human_readable_and_from_total() {
        let report = build_report(&fixture());
        let row = report.rows.iter().find(|r| r[0] == "myindex").unwrap();
        assert_eq!(row[2], "2.0MB");
    }

    #[test]
    fn indexing_and_search_totals_come_from_total() {
        let report = build_report(&fixture());
        let row = report.rows.iter().find(|r| r[0] == "myindex").unwrap();
        assert_eq!(row[3], "250");
        assert_eq!(row[4], "42");
    }

    #[test]
    fn missing_indices_object_yields_empty_report() {
        let report = build_report(&json!({}));
        assert!(report.rows.is_empty());
        assert_eq!(report.columns, vec!["index", "docs.count", "store.size", "indexing.index_total", "search.query_total"]);
    }

    #[test]
    fn missing_metric_fields_render_as_placeholder() {
        let body = json!({"indices": {"bare": {}}});
        let report = build_report(&body);
        assert_eq!(report.rows[0], vec!["bare", "-", "-", "-", "-"]);
    }
}
