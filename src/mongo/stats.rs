//! `dbops mongo stats [--db <name>]`.
//!
//! Without `--db`: a per-database summary, `listDatabases` (name,
//! sizeOnDisk) joined with one `dbStats` call per database (collections,
//! objects) so every row has the same shape regardless of what a given
//! database happens to report.
//!
//! With `--db <name>`: `dbStats` for the database as a whole plus one
//! `collStats` call per collection (count, size, storageSize,
//! totalIndexSize, avgObjSize), rendered as a single table with the db-wide
//! totals as a trailing `TOTAL` row.

use anyhow::{Context, Result};
use mongodb::bson::{doc, Document};
use mongodb::{Client, Database};

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::mongo::client;

pub async fn run(ctx: &Ctx, db: Option<&str>) -> Result<ExitCode> {
    let mongo_client = match client::connect(&ctx.profile.mongodb, ctx.timeout, ctx.insecure).await
    {
        Ok(mongo_client) => mongo_client,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let fetched = match db {
        Some(name) => tokio::time::timeout(ctx.timeout, fetch_db_report(&mongo_client, name)).await,
        None => tokio::time::timeout(ctx.timeout, fetch_overview_report(&mongo_client)).await,
    };

    match fetched {
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
                "error: mongodb stats query timed out after {:?}",
                ctx.timeout
            );
            Ok(ExitCode::from(unix::CONNECTION_FAILED))
        }
    }
}

// --- overview (no --db) -----------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct DbSummary {
    name: String,
    size_on_disk: i64,
    /// Filled in from a per-db `dbStats` follow-up call; `None` if that call
    /// failed (e.g. no permission on that database) rather than failing the
    /// whole report.
    collections: Option<i64>,
    objects: Option<i64>,
}

async fn fetch_overview_report(mongo_client: &Client) -> Result<StatReport> {
    let list_doc = mongo_client
        .database("admin")
        .run_command(doc! { "listDatabases": 1 })
        .await
        .context("listDatabases failed")?;
    let mut summaries = parse_list_databases(&list_doc)?;

    for summary in &mut summaries {
        let db = mongo_client.database(&summary.name);
        if let Ok(stats_doc) = db.run_command(doc! { "dbStats": 1 }).await {
            let totals = parse_db_totals(&stats_doc);
            summary.collections = totals.collections;
            summary.objects = totals.objects;
        }
    }

    Ok(build_overview_report(&summaries))
}

/// Pure: raw `listDatabases` response -> one summary per database, with
/// `collections`/`objects` left unset (the caller fills those in via a
/// follow-up `dbStats` call per database).
fn parse_list_databases(doc: &Document) -> Result<Vec<DbSummary>> {
    let entries = doc
        .get_array("databases")
        .context("listDatabases response is missing 'databases'")?;
    let mut summaries = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry
            .as_document()
            .context("listDatabases 'databases' entry is not a document")?;
        let name = entry
            .get_str("name")
            .context("database entry is missing 'name'")?
            .to_string();
        let size_on_disk = bson_i64(entry, "sizeOnDisk").unwrap_or(0);
        summaries.push(DbSummary {
            name,
            size_on_disk,
            collections: None,
            objects: None,
        });
    }
    Ok(summaries)
}

fn build_overview_report(summaries: &[DbSummary]) -> StatReport {
    let rows = summaries
        .iter()
        .map(|s| {
            vec![
                s.name.clone(),
                s.size_on_disk.to_string(),
                s.collections
                    .map_or_else(|| "-".to_string(), |v| v.to_string()),
                s.objects.map_or_else(|| "-".to_string(), |v| v.to_string()),
            ]
        })
        .collect();

    StatReport {
        columns: vec![
            "database".to_string(),
            "sizeOnDisk".to_string(),
            "collections".to_string(),
            "objects".to_string(),
        ],
        rows,
    }
}

// --- per-database detail (--db <name>) ---------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct DbTotals {
    collections: Option<i64>,
    objects: Option<i64>,
    data_size: Option<i64>,
    storage_size: Option<i64>,
    index_size: Option<i64>,
    avg_obj_size: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
struct CollStat {
    name: String,
    count: Option<i64>,
    size: Option<i64>,
    storage_size: Option<i64>,
    total_index_size: Option<i64>,
    avg_obj_size: Option<i64>,
}

impl CollStat {
    fn unavailable(name: String) -> Self {
        Self {
            name,
            count: None,
            size: None,
            storage_size: None,
            total_index_size: None,
            avg_obj_size: None,
        }
    }
}

async fn fetch_db_report(mongo_client: &Client, db_name: &str) -> Result<StatReport> {
    let db: Database = mongo_client.database(db_name);
    let stats_doc = db
        .run_command(doc! { "dbStats": 1 })
        .await
        .context("dbStats failed")?;
    let totals = parse_db_totals(&stats_doc);

    let names = db
        .list_collection_names()
        .await
        .context("failed to list collections")?;
    let mut collections = Vec::with_capacity(names.len());
    for name in names {
        // A failing collStats (e.g. run against a view, which doesn't
        // support it) becomes an "unavailable" row instead of failing the
        // whole report. One uncooperative collection shouldn't hide every
        // other collection's stats.
        let stat = match db.run_command(doc! { "collStats": name.as_str() }).await {
            Ok(coll_doc) => parse_coll_stats(&name, &coll_doc),
            Err(_err) => CollStat::unavailable(name.clone()),
        };
        collections.push(stat);
    }
    collections.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(build_db_report(&collections, &totals))
}

/// Pure: raw `dbStats` response -> the fields this command cares about.
/// Never fails: an unrecognized/missing field just renders as `-` later,
/// matching [`crate::mongo::connections`]'s tolerant-parsing style.
fn parse_db_totals(doc: &Document) -> DbTotals {
    DbTotals {
        collections: bson_i64(doc, "collections"),
        objects: bson_i64(doc, "objects"),
        data_size: bson_i64(doc, "dataSize"),
        storage_size: bson_i64(doc, "storageSize"),
        index_size: bson_i64(doc, "indexSize"),
        avg_obj_size: bson_i64(doc, "avgObjSize"),
    }
}

/// Pure: raw `collStats` response for one collection -> the PRD's five
/// fields (count, size, storageSize, totalIndexSize, avgObjSize).
fn parse_coll_stats(name: &str, doc: &Document) -> CollStat {
    CollStat {
        name: name.to_string(),
        count: bson_i64(doc, "count"),
        size: bson_i64(doc, "size"),
        storage_size: bson_i64(doc, "storageSize"),
        total_index_size: bson_i64(doc, "totalIndexSize"),
        avg_obj_size: bson_i64(doc, "avgObjSize"),
    }
}

fn build_db_report(collections: &[CollStat], totals: &DbTotals) -> StatReport {
    let value_or_dash = |v: Option<i64>| v.map_or_else(|| "-".to_string(), |v| v.to_string());

    let mut rows: Vec<Vec<String>> = collections
        .iter()
        .map(|c| {
            vec![
                c.name.clone(),
                value_or_dash(c.count),
                value_or_dash(c.size),
                value_or_dash(c.storage_size),
                value_or_dash(c.total_index_size),
                value_or_dash(c.avg_obj_size),
            ]
        })
        .collect();

    rows.push(vec![
        "TOTAL".to_string(),
        value_or_dash(totals.objects),
        value_or_dash(totals.data_size),
        value_or_dash(totals.storage_size),
        value_or_dash(totals.index_size),
        value_or_dash(totals.avg_obj_size),
    ]);

    StatReport {
        columns: vec![
            "collection".to_string(),
            "count".to_string(),
            "size".to_string(),
            "storageSize".to_string(),
            "totalIndexSize".to_string(),
            "avgObjSize".to_string(),
        ],
        rows,
    }
}

/// Tolerates `Int32`, `Int64`, or `Double` wire representations: `dbStats`
/// and `collStats` field types vary by MongoDB version.
fn bson_i64(doc: &Document, key: &str) -> Option<i64> {
    doc.get_i64(key)
        .ok()
        .or_else(|| doc.get_i32(key).ok().map(i64::from))
        .or_else(|| doc.get_f64(key).ok().map(|v| v as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_databases_reads_name_and_size() {
        let doc = doc! {
            "databases": [
                { "name": "admin", "sizeOnDisk": 40_960i64, "empty": false },
                { "name": "app", "sizeOnDisk": 1_048_576i64, "empty": false },
            ],
            "totalSize": 1_089_536i64,
        };
        let summaries = parse_list_databases(&doc).unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(
            summaries[1],
            DbSummary {
                name: "app".to_string(),
                size_on_disk: 1_048_576,
                collections: None,
                objects: None
            }
        );
    }

    #[test]
    fn parse_list_databases_requires_databases_array() {
        let err = parse_list_databases(&doc! { "ok": 1.0 }).unwrap_err();
        assert!(err.to_string().contains("databases"));
    }

    #[test]
    fn build_overview_report_renders_dash_for_unset_totals() {
        let summaries = vec![DbSummary {
            name: "app".to_string(),
            size_on_disk: 100,
            collections: None,
            objects: None,
        }];
        let report = build_overview_report(&summaries);
        assert_eq!(
            report.columns,
            vec!["database", "sizeOnDisk", "collections", "objects"]
        );
        assert_eq!(report.rows[0], vec!["app", "100", "-", "-"]);
    }

    #[test]
    fn build_overview_report_fills_in_totals_when_present() {
        let summaries = vec![DbSummary {
            name: "app".to_string(),
            size_on_disk: 100,
            collections: Some(3),
            objects: Some(42),
        }];
        let report = build_overview_report(&summaries);
        assert_eq!(report.rows[0], vec!["app", "100", "3", "42"]);
    }

    #[test]
    fn parse_db_totals_reads_known_fields() {
        let doc = doc! {
            "db": "app",
            "collections": 3i64,
            "objects": 42i64,
            "dataSize": 8_192.0,
            "storageSize": 16_384i64,
            "indexSize": 4_096i64,
            "avgObjSize": 195.0,
        };
        let totals = parse_db_totals(&doc);
        assert_eq!(
            totals,
            DbTotals {
                collections: Some(3),
                objects: Some(42),
                data_size: Some(8_192),
                storage_size: Some(16_384),
                index_size: Some(4_096),
                avg_obj_size: Some(195),
            }
        );
    }

    #[test]
    fn parse_coll_stats_reads_the_five_prd_fields() {
        let doc = doc! {
            "ns": "app.users",
            "count": 10i64,
            "size": 2_000i64,
            "storageSize": 4_096i64,
            "totalIndexSize": 1_024i64,
            "avgObjSize": 200i64,
        };
        let stat = parse_coll_stats("users", &doc);
        assert_eq!(
            stat,
            CollStat {
                name: "users".to_string(),
                count: Some(10),
                size: Some(2_000),
                storage_size: Some(4_096),
                total_index_size: Some(1_024),
                avg_obj_size: Some(200),
            }
        );
    }

    #[test]
    fn build_db_report_appends_a_total_row_from_db_stats() {
        let collections = vec![CollStat {
            name: "users".to_string(),
            count: Some(10),
            size: Some(2_000),
            storage_size: Some(4_096),
            total_index_size: Some(1_024),
            avg_obj_size: Some(200),
        }];
        let totals = DbTotals {
            collections: Some(1),
            objects: Some(10),
            data_size: Some(2_000),
            storage_size: Some(4_096),
            index_size: Some(1_024),
            avg_obj_size: Some(200),
        };
        let report = build_db_report(&collections, &totals);
        assert_eq!(report.rows.len(), 2);
        assert_eq!(
            report.rows[1],
            vec!["TOTAL", "10", "2000", "4096", "1024", "200"]
        );
    }

    #[test]
    fn build_db_report_renders_dash_for_an_unavailable_collection() {
        let collections = vec![CollStat::unavailable("a_view".to_string())];
        let report = build_db_report(&collections, &DbTotals::default());
        assert_eq!(report.rows[0], vec!["a_view", "-", "-", "-", "-", "-"]);
    }
}
