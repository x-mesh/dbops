//! `dbops pg stats [--db <name>]`: database size (`pg_size_pretty`),
//! connection count against `max_connections`, and idle-in-transaction
//! session count.
//!
//! Every metric except `max_connections` (a server-wide GUC, has no per-db
//! meaning) is scoped to the resolved database: `--db` if given, else
//! whatever `current_database()` resolves to on the connected session.

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::pg::client;

pub async fn run(ctx: &Ctx, db: Option<&str>) -> Result<ExitCode> {
    let pg_client = match client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let data = match tokio::time::timeout(ctx.timeout, fetch(&pg_client, db)).await {
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

    println!("{}", output::render_stat(&build_report(&data), ctx.json));
    Ok(ExitCode::from(unix::SUCCESS))
}

struct StatsData {
    database: String,
    db_size_pretty: String,
    connections_used: i64,
    connections_max: i64,
    idle_in_transaction: i64,
}

async fn fetch(pg_client: &Client, db: Option<&str>) -> Result<StatsData> {
    // Resolved in Rust rather than via `COALESCE($1, current_database())` in
    // SQL: `current_database()` returns `name`, not `text`, and mixing
    // that with a `pg_database_size(text)` overload call is exactly the kind
    // of implicit-cast ambiguity worth avoiding rather than debugging.
    let database = match db {
        Some(name) => name.to_string(),
        None => pg_client
            .query_one("SELECT current_database()", &[])
            .await
            .context("current_database() query failed")?
            .try_get(0)
            .context("unexpected current_database() row shape")?,
    };

    let db_size_pretty: String = pg_client
        .query_one("SELECT pg_size_pretty(pg_database_size($1))", &[&database])
        .await
        .context("pg_database_size query failed")?
        .try_get(0)
        .context("unexpected pg_database_size row shape")?;

    let connections_used: i64 = pg_client
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = $1",
            &[&database],
        )
        .await
        .context("pg_stat_activity connection count query failed")?
        .try_get(0)
        .context("unexpected connection count row shape")?;

    let max_raw: String = pg_client
        .query_one("SHOW max_connections", &[])
        .await
        .context("SHOW max_connections failed")?
        .try_get(0)
        .context("unexpected max_connections row shape")?;
    let connections_max: i64 = max_raw
        .trim()
        .parse()
        .context("failed to parse max_connections")?;

    let idle_in_transaction: i64 = pg_client
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND state = 'idle in transaction'",
            &[&database],
        )
        .await
        .context("idle-in-transaction count query failed")?
        .try_get(0)
        .context("unexpected idle-in-transaction row shape")?;

    Ok(StatsData {
        database,
        db_size_pretty,
        connections_used,
        connections_max,
        idle_in_transaction,
    })
}

/// Pure: fetched values -> the rendered field/value report. `connections_pct`
/// is `-` rather than a fabricated `0.0%` when `max_connections` itself is
/// unreadable (0), since dividing by it would misreport certainty.
fn build_report(data: &StatsData) -> StatReport {
    let connections_pct = if data.connections_max == 0 {
        "-".to_string()
    } else {
        format!(
            "{:.1}%",
            data.connections_used as f64 / data.connections_max as f64 * 100.0
        )
    };

    StatReport {
        columns: vec!["metric".to_string(), "value".to_string()],
        rows: vec![
            vec!["database".to_string(), data.database.clone()],
            vec!["db_size".to_string(), data.db_size_pretty.clone()],
            vec![
                "connections_used".to_string(),
                data.connections_used.to_string(),
            ],
            vec![
                "connections_max".to_string(),
                data.connections_max.to_string(),
            ],
            vec!["connections_pct".to_string(), connections_pct],
            vec![
                "idle_in_transaction".to_string(),
                data.idle_in_transaction.to_string(),
            ],
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(connections_used: i64, connections_max: i64, idle: i64) -> StatsData {
        StatsData {
            database: "app".to_string(),
            db_size_pretty: "23 MB".to_string(),
            connections_used,
            connections_max,
            idle_in_transaction: idle,
        }
    }

    #[test]
    fn builds_expected_rows() {
        let report = build_report(&sample(42, 200, 3));
        assert_eq!(report.columns, vec!["metric", "value"]);
        assert!(report
            .rows
            .contains(&vec!["database".to_string(), "app".to_string()]));
        assert!(report
            .rows
            .contains(&vec!["db_size".to_string(), "23 MB".to_string()]));
        assert!(report
            .rows
            .contains(&vec!["connections_used".to_string(), "42".to_string()]));
        assert!(report
            .rows
            .contains(&vec!["connections_max".to_string(), "200".to_string()]));
        assert!(report
            .rows
            .contains(&vec!["connections_pct".to_string(), "21.0%".to_string()]));
        assert!(report
            .rows
            .contains(&vec!["idle_in_transaction".to_string(), "3".to_string()]));
    }

    #[test]
    fn zero_max_connections_is_dash_not_fabricated_zero() {
        let report = build_report(&sample(0, 0, 0));
        assert!(report
            .rows
            .contains(&vec!["connections_pct".to_string(), "-".to_string()]));
    }
}
