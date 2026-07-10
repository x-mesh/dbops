//! `dbops mongo oplog` — the oplog window (time between the oldest and
//! newest entries in `local.oplog.rs`) plus its capped-collection
//! size/usage.
//!
//! Standalone nodes (and replica-set members before `rs.initiate()`) have no
//! `local.oplog.rs` at all -- that is reported plainly and exits 0, not an
//! error, since "no oplog" is a valid, expected topology (PRD edge case).

use anyhow::{Context, Result};
use mongodb::bson::{doc, Document};
use mongodb::Client;

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::mongo::client;

const OPLOG_COLLECTION: &str = "oplog.rs";

pub async fn run(ctx: &Ctx) -> Result<ExitCode> {
    let mongo_client = match client::connect(&ctx.profile.mongodb, ctx.timeout, ctx.insecure).await
    {
        Ok(mongo_client) => mongo_client,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    match tokio::time::timeout(ctx.timeout, fetch_report(&mongo_client)).await {
        Ok(Ok(report)) => {
            println!("{}", output::render_stat(&report, ctx.json));
            Ok(ExitCode::from(unix::SUCCESS))
        }
        Ok(Err(err)) => {
            eprintln!("error: {err:#}");
            Ok(ExitCode::from(unix::CONNECTION_FAILED))
        }
        Err(_elapsed) => {
            eprintln!(
                "error: mongodb oplog query timed out after {:?}",
                ctx.timeout
            );
            Ok(ExitCode::from(unix::CONNECTION_FAILED))
        }
    }
}

async fn fetch_report(mongo_client: &Client) -> Result<StatReport> {
    let local_db = mongo_client.database("local");
    let names = local_db
        .list_collection_names()
        .await
        .context("failed to list collections in 'local'")?;
    if !names.iter().any(|n| n == OPLOG_COLLECTION) {
        return Ok(standalone_report());
    }

    let stats_doc = local_db
        .run_command(doc! { "collStats": OPLOG_COLLECTION })
        .await
        .context("collStats on local.oplog.rs failed")?;
    let usage = parse_oplog_usage(&stats_doc)?;

    let collection = local_db.collection::<Document>(OPLOG_COLLECTION);
    let oldest = collection
        .find_one(doc! {})
        .sort(doc! { "$natural": 1 })
        .await
        .context("failed to read the oldest oplog entry")?
        .context("local.oplog.rs has no entries yet")?;
    let newest = collection
        .find_one(doc! {})
        .sort(doc! { "$natural": -1 })
        .await
        .context("failed to read the newest oplog entry")?
        .context("local.oplog.rs has no entries yet")?;
    let window_secs = parse_window_secs(&oldest, &newest)?;

    Ok(build_report(window_secs, &usage))
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct OplogUsage {
    size: Option<i64>,
    storage_size: Option<i64>,
    max_size: Option<i64>,
    count: Option<i64>,
}

/// Pure: raw `collStats` response for `local.oplog.rs` -> the fields this
/// command cares about. `maxSize` in particular is only present on capped
/// collections (which the oplog always is), but tolerate its absence rather
/// than failing the whole report over one field.
fn parse_oplog_usage(doc: &Document) -> Result<OplogUsage> {
    Ok(OplogUsage {
        size: bson_i64(doc, "size"),
        storage_size: bson_i64(doc, "storageSize"),
        max_size: bson_i64(doc, "maxSize"),
        count: bson_i64(doc, "count"),
    })
}

/// Pure: the oldest and newest oplog documents -> the window between them,
/// in seconds. Kept free of any `mongodb::Client` dependency so it can be
/// unit-tested against mock documents.
fn parse_window_secs(oldest: &Document, newest: &Document) -> Result<u64> {
    let oldest_ts = oldest
        .get_timestamp("ts")
        .context("oplog entry is missing 'ts'")?;
    let newest_ts = newest
        .get_timestamp("ts")
        .context("oplog entry is missing 'ts'")?;
    Ok(newest_ts.time.saturating_sub(oldest_ts.time) as u64)
}

/// Tolerates `Int32`, `Int64`, or `Double` wire representations.
fn bson_i64(doc: &Document, key: &str) -> Option<i64> {
    doc.get_i64(key)
        .ok()
        .or_else(|| doc.get_i32(key).ok().map(i64::from))
        .or_else(|| doc.get_f64(key).ok().map(|v| v as i64))
}

/// `total_secs` -> `"1d 2h 3m 4s"`, dropping leading zero units but always
/// keeping at least one (`"0s"` for a zero-width window).
fn format_duration(total_secs: u64) -> String {
    if total_secs == 0 {
        return "0s".to_string();
    }
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }
    parts.join(" ")
}

/// Pure: parsed window + usage -> the rendered table. The `metric`/`value`
/// column shape matches [`standalone_report`] so `--json` output has a
/// consistent shape regardless of which branch ran.
fn build_report(window_secs: u64, usage: &OplogUsage) -> StatReport {
    let value_or_dash = |v: Option<i64>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
    StatReport {
        columns: vec!["metric".to_string(), "value".to_string()],
        rows: vec![
            vec!["window".to_string(), format_duration(window_secs)],
            vec!["window_seconds".to_string(), window_secs.to_string()],
            vec!["size_bytes".to_string(), value_or_dash(usage.size)],
            vec![
                "storage_size_bytes".to_string(),
                value_or_dash(usage.storage_size),
            ],
            vec!["max_size_bytes".to_string(), value_or_dash(usage.max_size)],
            vec!["count".to_string(), value_or_dash(usage.count)],
        ],
    }
}

fn standalone_report() -> StatReport {
    StatReport {
        columns: vec!["metric".to_string(), "value".to_string()],
        rows: vec![
            vec!["status".to_string(), "standalone".to_string()],
            vec![
                "note".to_string(),
                "not a replica set (no oplog found)".to_string(),
            ],
        ],
    }
}

#[cfg(test)]
mod tests {
    use mongodb::bson::Timestamp;

    use super::*;

    fn ts_doc(secs: u32) -> Document {
        doc! { "ts": Timestamp { time: secs, increment: 1 }, "op": "i" }
    }

    #[test]
    fn parse_window_secs_computes_the_gap() {
        let oldest = ts_doc(1_700_000_000);
        let newest = ts_doc(1_700_003_600);
        assert_eq!(parse_window_secs(&oldest, &newest).unwrap(), 3_600);
    }

    #[test]
    fn parse_window_secs_requires_ts() {
        let err = parse_window_secs(&doc! {}, &ts_doc(1)).unwrap_err();
        assert!(err.to_string().contains("ts"));
    }

    #[test]
    fn parse_oplog_usage_reads_known_fields() {
        let doc = doc! { "size": 1_048_576i64, "storageSize": 2_097_152i64, "maxSize": 10_485_760i64, "count": 42i64 };
        let usage = parse_oplog_usage(&doc).unwrap();
        assert_eq!(
            usage,
            OplogUsage {
                size: Some(1_048_576),
                storage_size: Some(2_097_152),
                max_size: Some(10_485_760),
                count: Some(42)
            }
        );
    }

    #[test]
    fn parse_oplog_usage_tolerates_missing_fields() {
        let usage = parse_oplog_usage(&doc! {}).unwrap();
        assert_eq!(usage, OplogUsage::default());
    }

    #[test]
    fn format_duration_zero_is_0s() {
        assert_eq!(format_duration(0), "0s");
    }

    #[test]
    fn format_duration_drops_leading_zero_units() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3_661), "1h 1m 1s");
        assert_eq!(format_duration(90_061), "1d 1h 1m 1s");
    }

    #[test]
    fn standalone_report_has_the_same_column_shape_as_the_healthy_path() {
        let healthy = build_report(0, &OplogUsage::default());
        let standalone = standalone_report();
        assert_eq!(healthy.columns, standalone.columns);
    }
}
