//! `dbops pg queries --long-running [--threshold <dur>]` (default 5m):
//! active sessions running longer than the threshold, from
//! `pg_stat_activity`. Without `--long-running` every active session is
//! listed unfiltered (threshold is ignored). Query text is truncated to
//! [`MAX_QUERY_CHARS`] here -- `frame::output` only escapes control
//! characters, it never shortens a value.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::frame::ctx::parse_timeout;
use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::pg::client;

const DEFAULT_THRESHOLD: Duration = Duration::from_secs(300);
const MAX_QUERY_CHARS: usize = 500;

const QUERY_FILTERED: &str = "\
SELECT pid, usename, datname, EXTRACT(EPOCH FROM (now() - query_start))::float8, query \
FROM pg_stat_activity \
WHERE state = 'active' AND query_start IS NOT NULL AND pid <> pg_backend_pid() \
  AND now() - query_start > ($1 * interval '1 second') \
ORDER BY 4 DESC";

const QUERY_ALL_ACTIVE: &str = "\
SELECT pid, usename, datname, EXTRACT(EPOCH FROM (now() - query_start))::float8, query \
FROM pg_stat_activity \
WHERE state = 'active' AND query_start IS NOT NULL AND pid <> pg_backend_pid() \
ORDER BY 4 DESC";

pub async fn run(ctx: &Ctx, long_running: bool, threshold: Option<&str>) -> Result<ExitCode> {
    let threshold = match threshold {
        Some(raw) => parse_timeout(raw).context("invalid --threshold value")?,
        None => DEFAULT_THRESHOLD,
    };

    let pg_client = match client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let rows =
        match tokio::time::timeout(ctx.timeout, fetch(&pg_client, long_running, threshold)).await {
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

struct QueryRow {
    pid: i32,
    user: Option<String>,
    db: Option<String>,
    duration_s: f64,
    query: String,
}

async fn fetch(
    pg_client: &Client,
    long_running: bool,
    threshold: Duration,
) -> Result<Vec<QueryRow>> {
    let rows = if long_running {
        let threshold_secs = threshold.as_secs_f64();
        pg_client
            .query(QUERY_FILTERED, &[&threshold_secs])
            .await
            .context("pg_stat_activity long-running query failed")?
    } else {
        pg_client
            .query(QUERY_ALL_ACTIVE, &[])
            .await
            .context("pg_stat_activity query failed")?
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(QueryRow {
            pid: row
                .try_get(0)
                .context("unexpected pg_stat_activity row shape")?,
            user: row
                .try_get(1)
                .context("unexpected pg_stat_activity row shape")?,
            db: row
                .try_get(2)
                .context("unexpected pg_stat_activity row shape")?,
            duration_s: row
                .try_get(3)
                .context("unexpected pg_stat_activity row shape")?,
            query: row
                .try_get(4)
                .context("unexpected pg_stat_activity row shape")?,
        });
    }
    Ok(out)
}

fn build_report(rows: &[QueryRow]) -> StatReport {
    let columns = ["pid", "user", "db", "duration", "query"]
        .into_iter()
        .map(String::from)
        .collect();
    let out_rows = rows
        .iter()
        .map(|r| {
            vec![
                r.pid.to_string(),
                r.user.clone().unwrap_or_else(|| "-".to_string()),
                r.db.clone().unwrap_or_else(|| "-".to_string()),
                format!("{:.1}s", r.duration_s),
                truncate_query(&r.query),
            ]
        })
        .collect();
    StatReport {
        columns,
        rows: out_rows,
    }
}

/// Char-boundary-safe truncation to [`MAX_QUERY_CHARS`]; control-character
/// escaping still happens downstream in `frame::output`.
fn truncate_query(query: &str) -> String {
    if query.chars().count() <= MAX_QUERY_CHARS {
        return query.to_string();
    }
    let mut truncated: String = query.chars().take(MAX_QUERY_CHARS).collect();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i32, duration_s: f64, query: &str) -> QueryRow {
        QueryRow {
            pid,
            user: Some("app".to_string()),
            db: Some("appdb".to_string()),
            duration_s,
            query: query.to_string(),
        }
    }

    #[test]
    fn formats_duration_and_identity() {
        let report = build_report(&[row(123, 90.25, "SELECT 1")]);
        assert_eq!(
            report.columns,
            vec!["pid", "user", "db", "duration", "query"]
        );
        assert_eq!(
            report.rows[0],
            vec!["123", "app", "appdb", "90.2s", "SELECT 1"]
        );
    }

    #[test]
    fn missing_user_and_db_render_as_placeholder() {
        let mut r = row(1, 1.0, "SELECT 1");
        r.user = None;
        r.db = None;
        let report = build_report(&[r]);
        assert_eq!(report.rows[0][1], "-");
        assert_eq!(report.rows[0][2], "-");
    }

    #[test]
    fn query_text_over_limit_is_truncated() {
        let long_query = "a".repeat(600);
        let report = build_report(&[row(1, 1.0, &long_query)]);
        assert_eq!(report.rows[0][4].chars().count(), MAX_QUERY_CHARS + 3);
        assert!(report.rows[0][4].ends_with("..."));
    }

    #[test]
    fn query_text_at_or_under_limit_is_untouched() {
        let query = "a".repeat(MAX_QUERY_CHARS);
        let report = build_report(&[row(1, 1.0, &query)]);
        assert_eq!(report.rows[0][4], query);
    }
}
