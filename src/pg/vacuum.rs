//! `dbops pg vacuum`: last (auto)vacuum timestamps plus the transaction-ID
//! age of every user table, ranked by wraparound risk (`age(relfrozenxid)`
//! descending) and compared against `autovacuum_freeze_max_age`.
//!
//! Ranking is capped at [`DEFAULT_TOP`] rather than exposed as a flag:
//! this command answers "what's closest to a wraparound-forced vacuum", a
//! fixed top-N question, not an open listing like `tables --top`.

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::pg::client;

const DEFAULT_TOP: i64 = 20;

const QUERY: &str = "\
SELECT n.nspname, c.relname, s.last_vacuum::text, s.last_autovacuum::text, age(c.relfrozenxid) \
FROM pg_class c \
JOIN pg_namespace n ON n.oid = c.relnamespace \
LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid \
WHERE c.relkind IN ('r', 'm') \
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
ORDER BY age(c.relfrozenxid) DESC \
LIMIT $1";

pub async fn run(ctx: &Ctx) -> Result<ExitCode> {
    let pg_client = match client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let (rows, freeze_max_age) = match tokio::time::timeout(ctx.timeout, fetch(&pg_client)).await {
        Ok(Ok(data)) => data,
        Ok(Err(err)) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
        Err(_) => {
            eprintln!("error: query timed out after {:?}", ctx.timeout);
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    println!(
        "{}",
        output::render_stat(&build_report(&rows, freeze_max_age), ctx.json)
    );
    Ok(ExitCode::from(unix::SUCCESS))
}

struct VacuumRow {
    schema: String,
    table: String,
    last_vacuum: Option<String>,
    last_autovacuum: Option<String>,
    xid_age: i32,
}

async fn fetch(pg_client: &Client) -> Result<(Vec<VacuumRow>, i64)> {
    let rows = pg_client
        .query(QUERY, &[&DEFAULT_TOP])
        .await
        .context("wraparound-age query failed")?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(VacuumRow {
            schema: row
                .try_get(0)
                .context("unexpected wraparound-age row shape")?,
            table: row
                .try_get(1)
                .context("unexpected wraparound-age row shape")?,
            last_vacuum: row
                .try_get(2)
                .context("unexpected wraparound-age row shape")?,
            last_autovacuum: row
                .try_get(3)
                .context("unexpected wraparound-age row shape")?,
            xid_age: row
                .try_get(4)
                .context("unexpected wraparound-age row shape")?,
        });
    }

    let freeze_max_age_raw: String = pg_client
        .query_one("SHOW autovacuum_freeze_max_age", &[])
        .await
        .context("SHOW autovacuum_freeze_max_age failed")?
        .try_get(0)
        .context("unexpected autovacuum_freeze_max_age row shape")?;
    let freeze_max_age: i64 = freeze_max_age_raw
        .trim()
        .parse()
        .context("failed to parse autovacuum_freeze_max_age")?;

    Ok((out, freeze_max_age))
}

/// Pure: fetched rows + the freeze-max-age GUC -> the rendered report.
/// `freeze_pct` is `-` when the GUC itself is unreadable (0), matching the
/// same "don't fabricate a ratio against nothing" rule used elsewhere.
fn build_report(rows: &[VacuumRow], freeze_max_age: i64) -> StatReport {
    let columns = [
        "table",
        "last_vacuum",
        "last_autovacuum",
        "xid_age",
        "freeze_pct",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let out_rows = rows
        .iter()
        .map(|r| {
            let freeze_pct = if freeze_max_age == 0 {
                "-".to_string()
            } else {
                format!("{:.1}%", r.xid_age as f64 / freeze_max_age as f64 * 100.0)
            };
            vec![
                format!("{}.{}", r.schema, r.table),
                r.last_vacuum.clone().unwrap_or_else(|| "-".to_string()),
                r.last_autovacuum.clone().unwrap_or_else(|| "-".to_string()),
                r.xid_age.to_string(),
                freeze_pct,
            ]
        })
        .collect();

    StatReport {
        columns,
        rows: out_rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        schema: &str,
        table: &str,
        last_vacuum: Option<&str>,
        last_autovacuum: Option<&str>,
        xid_age: i32,
    ) -> VacuumRow {
        VacuumRow {
            schema: schema.to_string(),
            table: table.to_string(),
            last_vacuum: last_vacuum.map(String::from),
            last_autovacuum: last_autovacuum.map(String::from),
            xid_age,
        }
    }

    #[test]
    fn combines_schema_and_table_name() {
        let report = build_report(
            &[row(
                "public",
                "events",
                Some("2026-07-01 00:00:00+00"),
                None,
                100_000_000,
            )],
            200_000_000,
        );
        assert_eq!(
            report.columns,
            vec![
                "table",
                "last_vacuum",
                "last_autovacuum",
                "xid_age",
                "freeze_pct"
            ]
        );
        assert_eq!(report.rows[0][0], "public.events");
    }

    #[test]
    fn missing_timestamps_render_as_placeholder() {
        let report = build_report(
            &[row(
                "public",
                "events",
                Some("2026-07-01 00:00:00+00"),
                None,
                100_000_000,
            )],
            200_000_000,
        );
        assert_eq!(report.rows[0][1], "2026-07-01 00:00:00+00");
        assert_eq!(report.rows[0][2], "-");
    }

    #[test]
    fn computes_freeze_pct() {
        let report = build_report(
            &[row("public", "events", None, None, 100_000_000)],
            200_000_000,
        );
        assert_eq!(report.rows[0][3], "100000000");
        assert_eq!(report.rows[0][4], "50.0%");
    }

    #[test]
    fn zero_freeze_max_age_is_dash_not_fabricated_zero() {
        let report = build_report(&[row("public", "events", None, None, 100)], 0);
        assert_eq!(report.rows[0][4], "-");
    }

    #[test]
    fn empty_input_yields_no_rows() {
        let report = build_report(&[], 200_000_000);
        assert!(report.rows.is_empty());
    }
}
