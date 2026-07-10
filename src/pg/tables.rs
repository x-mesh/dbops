//! `dbops pg tables --top N` (default 20): largest tables by total size
//! (`pg_total_relation_size`), split into table/index bytes, plus an
//! approximate bloat ratio derived from `pg_stat_user_tables.n_dead_tup`.
//!
//! `pgstattuple` isn't assumed installed, so this is an estimate, not exact
//! bloat -- hence the `dead_pct` column name (not `bloat_pct`).

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::pg::client;

const DEFAULT_TOP: u32 = 20;

const QUERY: &str = "\
SELECT schemaname, relname, \
       pg_size_pretty(pg_total_relation_size(relid)), \
       pg_size_pretty(pg_relation_size(relid)), \
       pg_size_pretty(pg_indexes_size(relid)), \
       n_live_tup, n_dead_tup \
FROM pg_stat_user_tables \
ORDER BY pg_total_relation_size(relid) DESC \
LIMIT $1";

pub async fn run(ctx: &Ctx, top: Option<u32>) -> Result<ExitCode> {
    let top = i64::from(top.unwrap_or(DEFAULT_TOP));

    let pg_client = match client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let rows = match tokio::time::timeout(ctx.timeout, fetch(&pg_client, top)).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(err)) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
        Err(_) => {
            eprintln!("error: query timed out after {:?}", ctx.timeout);
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    println!("{}", output::render_stat(&build_report(&rows), ctx.json));
    Ok(ExitCode::from(unix::SUCCESS))
}

struct TableRow {
    schema: String,
    table: String,
    total_size: String,
    table_size: String,
    index_size: String,
    live_tup: i64,
    dead_tup: i64,
}

async fn fetch(pg_client: &Client, top: i64) -> Result<Vec<TableRow>> {
    let rows = pg_client.query(QUERY, &[&top]).await.context("pg_stat_user_tables query failed")?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(TableRow {
            schema: row.try_get(0).context("unexpected pg_stat_user_tables row shape")?,
            table: row.try_get(1).context("unexpected pg_stat_user_tables row shape")?,
            total_size: row.try_get(2).context("unexpected pg_stat_user_tables row shape")?,
            table_size: row.try_get(3).context("unexpected pg_stat_user_tables row shape")?,
            index_size: row.try_get(4).context("unexpected pg_stat_user_tables row shape")?,
            live_tup: row.try_get(5).context("unexpected pg_stat_user_tables row shape")?,
            dead_tup: row.try_get(6).context("unexpected pg_stat_user_tables row shape")?,
        });
    }
    Ok(out)
}

/// Pure: fetched rows -> the rendered report. `dead_pct` is `-` (not
/// `0.0%`) when a table has never been touched (`live_tup + dead_tup == 0`)
/// -- dividing zero by zero would misrepresent "no data yet" as "no bloat".
fn build_report(rows: &[TableRow]) -> StatReport {
    let columns = ["table", "total_size", "table_size", "index_size", "dead_pct"]
        .into_iter()
        .map(String::from)
        .collect();

    let out_rows = rows
        .iter()
        .map(|r| {
            let total = r.live_tup + r.dead_tup;
            let dead_pct = if total == 0 {
                "-".to_string()
            } else {
                format!("{:.1}%", r.dead_tup as f64 / total as f64 * 100.0)
            };
            vec![
                format!("{}.{}", r.schema, r.table),
                r.total_size.clone(),
                r.table_size.clone(),
                r.index_size.clone(),
                dead_pct,
            ]
        })
        .collect();

    StatReport { columns, rows: out_rows }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(schema: &str, table: &str, live: i64, dead: i64) -> TableRow {
        TableRow {
            schema: schema.to_string(),
            table: table.to_string(),
            total_size: "10 MB".to_string(),
            table_size: "8 MB".to_string(),
            index_size: "2 MB".to_string(),
            live_tup: live,
            dead_tup: dead,
        }
    }

    #[test]
    fn combines_schema_and_table_name() {
        let report = build_report(&[row("public", "orders", 100, 10)]);
        assert_eq!(report.columns, vec!["table", "total_size", "table_size", "index_size", "dead_pct"]);
        assert_eq!(report.rows[0][0], "public.orders");
    }

    #[test]
    fn computes_dead_pct() {
        let report = build_report(&[row("public", "orders", 90, 10)]);
        assert_eq!(report.rows[0][4], "10.0%");
    }

    #[test]
    fn untouched_table_is_dash_not_zero() {
        let report = build_report(&[row("public", "empty", 0, 0)]);
        assert_eq!(report.rows[0][4], "-");
    }

    #[test]
    fn empty_input_yields_no_rows() {
        let report = build_report(&[]);
        assert!(report.rows.is_empty());
    }
}
