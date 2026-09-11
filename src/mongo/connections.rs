//! `dbops mongo connections`. `serverStatus().connections`: current,
//! available, totalCreated, and `active` when the server reports it (added
//! in MongoDB 4.0+; older servers simply omit the field).

use anyhow::{Context, Result};
use mongodb::bson::{doc, Document};
use mongodb::Client;

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::mongo::client;

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
                "error: mongodb connections query timed out after {:?}",
                ctx.timeout
            );
            Ok(ExitCode::from(unix::CONNECTION_FAILED))
        }
    }
}

async fn fetch_report(mongo_client: &Client) -> Result<StatReport> {
    let status_doc = mongo_client
        .database("admin")
        .run_command(doc! { "serverStatus": 1 })
        .await
        .context("serverStatus failed")?;
    let stats = parse_connections(&status_doc)?;
    Ok(build_report(&stats))
}

#[derive(Debug, Clone, PartialEq)]
struct ConnectionStats {
    current: i64,
    available: i64,
    total_created: i64,
    /// Only present on MongoDB 4.0+.
    active: Option<i64>,
}

/// Pure: raw `serverStatus` response -> the fields this command cares about.
/// Kept free of any `mongodb::Client` dependency so it can be unit-tested
/// against mock documents.
fn parse_connections(doc: &Document) -> Result<ConnectionStats> {
    let conns = doc
        .get_document("connections")
        .context("serverStatus response is missing a 'connections' section")?;

    Ok(ConnectionStats {
        current: bson_i64(conns, "current")
            .context("connections.current missing or not numeric")?,
        available: bson_i64(conns, "available")
            .context("connections.available missing or not numeric")?,
        total_created: bson_i64(conns, "totalCreated")
            .context("connections.totalCreated missing or not numeric")?,
        active: bson_i64(conns, "active"),
    })
}

/// Tolerates `Int32`, `Int64`, or `Double` wire representations -- which
/// field type `serverStatus` uses for a given counter varies by MongoDB
/// version.
fn bson_i64(doc: &Document, key: &str) -> Option<i64> {
    doc.get_i64(key)
        .ok()
        .or_else(|| doc.get_i32(key).ok().map(i64::from))
        .or_else(|| doc.get_f64(key).ok().map(|v| v as i64))
}

/// Pure: parsed stats -> the rendered table. Free of any BSON dependency so
/// it can be tested with plain Rust values.
fn build_report(stats: &ConnectionStats) -> StatReport {
    StatReport {
        columns: vec!["metric".to_string(), "value".to_string()],
        rows: vec![
            vec!["current".to_string(), stats.current.to_string()],
            vec!["available".to_string(), stats.available.to_string()],
            vec!["total_created".to_string(), stats.total_created.to_string()],
            vec![
                "active".to_string(),
                stats
                    .active
                    .map_or_else(|| "-".to_string(), |v| v.to_string()),
            ],
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_status_doc(active: Option<i32>) -> Document {
        let mut connections = doc! {
            "current": 12,
            "available": 838_848,
            "totalCreated": 57,
        };
        if let Some(active) = active {
            connections.insert("active", active);
        }
        doc! { "ok": 1.0, "connections": connections }
    }

    #[test]
    fn parse_connections_reads_all_four_fields() {
        let stats = parse_connections(&mock_status_doc(Some(3))).unwrap();
        assert_eq!(
            stats,
            ConnectionStats {
                current: 12,
                available: 838_848,
                total_created: 57,
                active: Some(3)
            }
        );
    }

    #[test]
    fn parse_connections_tolerates_missing_active() {
        let stats = parse_connections(&mock_status_doc(None)).unwrap();
        assert_eq!(stats.active, None);
    }

    #[test]
    fn parse_connections_requires_connections_section() {
        let err = parse_connections(&doc! { "ok": 1.0 }).unwrap_err();
        assert!(err.to_string().contains("connections"));
    }

    #[test]
    fn build_report_renders_dash_for_missing_active() {
        let stats = ConnectionStats {
            current: 1,
            available: 2,
            total_created: 3,
            active: None,
        };
        let report = build_report(&stats);
        assert_eq!(report.columns, vec!["metric", "value"]);
        assert_eq!(report.rows[3], vec!["active", "-"]);
    }

    #[test]
    fn build_report_renders_active_when_present() {
        let stats = ConnectionStats {
            current: 1,
            available: 2,
            total_created: 3,
            active: Some(1),
        };
        let report = build_report(&stats);
        assert_eq!(report.rows[3], vec!["active", "1"]);
    }
}
