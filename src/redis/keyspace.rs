//! `dbops redis keyspace`: `INFO keyspace` -> one row per logical DB with
//! its key count and expiring-key count.

use anyhow::Result;

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::redis::{connect, info};

pub async fn run(ctx: &Ctx) -> Result<ExitCode> {
    let mut conn = match connect::connect(ctx).await {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let raw = match connect::call::<String>(ctx, redis::cmd("INFO").arg("keyspace").query_async(&mut conn)).await {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let report = build_report(&raw);
    println!("{}", output::render_stat(&report, ctx.json));
    Ok(ExitCode::from(unix::SUCCESS))
}

/// Pure: `INFO keyspace` text -> one `StatReport` row per `dbN:` line,
/// ordered by DB index. A keyspace with no populated DBs yields zero rows
/// (comfy-table renders the header only).
fn build_report(raw: &str) -> StatReport {
    let fields = info::parse_info(raw);
    let mut dbs: Vec<(String, u32)> = fields
        .keys()
        .filter(|key| key.starts_with("db"))
        .filter_map(|key| db_index(key).map(|idx| (key.clone(), idx)))
        .collect();
    dbs.sort_by_key(|(_, idx)| *idx);

    let rows = dbs
        .into_iter()
        .map(|(key, _)| {
            let db_fields = info::parse_fields(&fields[&key]);
            vec![
                key,
                db_fields.get("keys").cloned().unwrap_or_else(|| "0".to_string()),
                db_fields.get("expires").cloned().unwrap_or_else(|| "0".to_string()),
            ]
        })
        .collect();

    StatReport {
        columns: vec!["db".to_string(), "keys".to_string(), "expires".to_string()],
        rows,
    }
}

/// `"db0"` -> `Some(0)`, `"db12"` -> `Some(12)`, anything else -> `None`
/// (guards against stray `dbfoo`-shaped keys that aren't real DB entries).
fn db_index(key: &str) -> Option<u32> {
    key.strip_prefix("db").and_then(|n| n.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
# Keyspace
db0:keys=5,expires=1,avg_ttl=0,subexpiry=0
db10:keys=100,expires=0,avg_ttl=0,subexpiry=0
db2:keys=7,expires=2,avg_ttl=120,subexpiry=0
";

    #[test]
    fn builds_one_row_per_db_sorted_numerically() {
        let report = build_report(FIXTURE);
        assert_eq!(report.columns, vec!["db", "keys", "expires"]);
        assert_eq!(report.rows, vec![
            vec!["db0", "5", "1"],
            vec!["db2", "7", "2"],
            vec!["db10", "100", "0"],
        ]);
    }

    #[test]
    fn empty_keyspace_yields_no_rows() {
        let report = build_report("# Keyspace\n");
        assert!(report.rows.is_empty());
    }
}
