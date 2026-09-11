//! `dbops os indices`: wraps `GET _cat/indices?format=json&bytes=b`.
//!
//! System indices (names starting with `.`, e.g. `.opendistro_security` /
//! `.kibana`) are excluded by default. They're operational plumbing, not
//! data an SRE is usually checking on. `--all` includes them.
//!
//! `bytes=b` requests raw byte counts from the server (rather than
//! server-side `kb`/`mb` rounding) so [`human_bytes`] can format
//! consistently regardless of index size.

use std::time::Duration;

use anyhow::{Context, Result};
use opensearch::cat::CatIndicesParts;
use opensearch::params::Bytes;
use opensearch::OpenSearch;
use serde_json::Value;

use crate::frame::result::StatReport;

const CAT_INDICES_FIELDS: &[&str] = &[
    "index",
    "health",
    "status",
    "docs.count",
    "pri.store.size",
    "store.size",
    "pri",
];

pub async fn indices(client: &OpenSearch, timeout: Duration, all: bool) -> Result<StatReport> {
    let entries = fetch(client, timeout).await?;
    Ok(build_report(&entries, all))
}

async fn fetch(client: &OpenSearch, timeout: Duration) -> Result<Vec<Value>> {
    let cat = client.cat();
    let fut = cat
        .indices(CatIndicesParts::None)
        .format("json")
        .bytes(Bytes::B)
        .h(CAT_INDICES_FIELDS)
        .send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("_cat/indices timed out")?
        .context("failed to query _cat/indices")?;
    let body: Vec<Value> = response
        .json()
        .await
        .context("failed to parse _cat/indices response")?;
    Ok(body)
}

/// Pure: raw `_cat/indices` JSON rows -> a sorted, filtered [`StatReport`].
/// `all=false` drops any index whose name starts with `.`.
fn build_report(entries: &[Value], all: bool) -> StatReport {
    let mut rows: Vec<Vec<String>> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("index").and_then(Value::as_str)?.to_string();
            if !all && name.starts_with('.') {
                return None;
            }

            let field = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_string()
            };
            let size_field = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u64>().ok())
                    .map_or_else(|| "-".to_string(), human_bytes)
            };

            Some(vec![
                name,
                field("health"),
                field("status"),
                field("docs.count"),
                size_field("pri.store.size"),
                size_field("store.size"),
                field("pri"),
            ])
        })
        .collect();

    rows.sort_by(|a, b| a[0].cmp(&b[0]));

    StatReport {
        columns: vec![
            "index",
            "health",
            "status",
            "docs.count",
            "pri.store.size",
            "store.size",
            "pri",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        rows,
    }
}

/// Byte count -> `"1.5GB"`-style string. Shared with [`crate::os::stats`]
/// (via `pub(super)`) so both commands render store sizes identically.
pub(super) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(
        index: &str,
        health: &str,
        docs: &str,
        pri_size: &str,
        total_size: &str,
        pri: &str,
    ) -> Value {
        json!({
            "index": index,
            "health": health,
            "status": "open",
            "docs.count": docs,
            "pri.store.size": pri_size,
            "store.size": total_size,
            "pri": pri,
        })
    }

    #[test]
    fn human_bytes_formats_common_sizes() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1024), "1.0KB");
        assert_eq!(human_bytes(1_048_576), "1.0MB");
        assert_eq!(human_bytes(1_610_612_736), "1.5GB");
    }

    #[test]
    fn system_indices_excluded_by_default() {
        let entries = vec![
            entry("myindex", "green", "10", "1024", "2048", "1"),
            entry(".kibana", "green", "1", "512", "512", "1"),
        ];
        let report = build_report(&entries, false);
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0][0], "myindex");
    }

    #[test]
    fn all_flag_includes_system_indices() {
        let entries = vec![
            entry("myindex", "green", "10", "1024", "2048", "1"),
            entry(".kibana", "green", "1", "512", "512", "1"),
        ];
        let report = build_report(&entries, true);
        assert_eq!(report.rows.len(), 2);
    }

    #[test]
    fn rows_are_sorted_by_index_name() {
        let entries = vec![
            entry("zzz", "green", "1", "1", "1", "1"),
            entry("aaa", "green", "1", "1", "1", "1"),
        ];
        let report = build_report(&entries, false);
        assert_eq!(report.rows[0][0], "aaa");
        assert_eq!(report.rows[1][0], "zzz");
    }

    #[test]
    fn store_size_fields_are_human_readable() {
        let entries = vec![entry("myindex", "green", "10", "1048576", "2097152", "1")];
        let report = build_report(&entries, false);
        assert_eq!(report.rows[0][4], "1.0MB");
        assert_eq!(report.rows[0][5], "2.0MB");
    }

    #[test]
    fn missing_fields_render_as_placeholder() {
        let entries = vec![json!({"index": "myindex"})];
        let report = build_report(&entries, false);
        assert_eq!(
            report.rows[0],
            vec!["myindex", "-", "-", "-", "-", "-", "-"]
        );
    }

    #[test]
    fn columns_match_prd_order() {
        let report = build_report(&[], false);
        assert_eq!(
            report.columns,
            vec![
                "index",
                "health",
                "status",
                "docs.count",
                "pri.store.size",
                "store.size",
                "pri"
            ]
        );
    }
}
