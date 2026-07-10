//! `dbops pg replication`: on a primary, `pg_stat_replication` per connected
//! standby (client address, WAL sender state, sent/replay LSN diff in
//! bytes, replay lag in seconds). On a standby (`pg_is_in_recovery()` true),
//! there's no equivalent per-standby view of *this* node's own lag -- that
//! comes from `pg_last_xact_replay_timestamp()` instead, so the report
//! shape switches to a role/value summary.

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::pg::client;

// `pg_wal_lsn_diff` returns `numeric`; cast to `bigint` since this crate
// doesn't enable tokio-postgres's decimal feature (no `rust_decimal`
// dependency), so `numeric` has no `FromSql` impl available here.
const PRIMARY_QUERY: &str = "\
SELECT client_addr::text, state, pg_wal_lsn_diff(sent_lsn, replay_lsn)::bigint, \
       EXTRACT(EPOCH FROM replay_lag)::float8 \
FROM pg_stat_replication \
ORDER BY 3 DESC NULLS LAST";

const STANDBY_QUERY: &str = "\
SELECT pg_last_xact_replay_timestamp()::text, \
       EXTRACT(EPOCH FROM (now() - pg_last_xact_replay_timestamp()))::float8";

pub async fn run(ctx: &Ctx) -> Result<ExitCode> {
    let pg_client = match client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let report = match tokio::time::timeout(ctx.timeout, fetch(&pg_client)).await {
        Ok(Ok(report)) => report,
        Ok(Err(err)) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
        Err(_) => {
            eprintln!("error: query timed out after {:?}", ctx.timeout);
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    println!("{}", output::render_stat(&report, ctx.json));
    Ok(ExitCode::from(unix::SUCCESS))
}

struct ReplicaRow {
    client_addr: Option<String>,
    state: String,
    lag_bytes: Option<i64>,
    replay_lag_s: Option<f64>,
}

async fn fetch(pg_client: &Client) -> Result<StatReport> {
    let in_recovery: bool = pg_client
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await
        .context("pg_is_in_recovery() query failed")?
        .try_get(0)
        .context("unexpected pg_is_in_recovery() row shape")?;

    if in_recovery {
        let row = pg_client
            .query_one(STANDBY_QUERY, &[])
            .await
            .context("pg_last_xact_replay_timestamp() query failed")?;
        let last_replay: Option<String> = row
            .try_get(0)
            .context("unexpected last-replay-timestamp row shape")?;
        let lag_s: Option<f64> = row.try_get(1).context("unexpected replay-lag row shape")?;
        return Ok(build_standby_report(last_replay.as_deref(), lag_s));
    }

    let rows = pg_client
        .query(PRIMARY_QUERY, &[])
        .await
        .context("pg_stat_replication query failed")?;
    let mut replicas = Vec::with_capacity(rows.len());
    for row in &rows {
        replicas.push(ReplicaRow {
            client_addr: row
                .try_get(0)
                .context("unexpected pg_stat_replication row shape")?,
            state: row
                .try_get(1)
                .context("unexpected pg_stat_replication row shape")?,
            lag_bytes: row
                .try_get(2)
                .context("unexpected pg_stat_replication row shape")?,
            replay_lag_s: row
                .try_get(3)
                .context("unexpected pg_stat_replication row shape")?,
        });
    }
    Ok(build_primary_report(&replicas))
}

fn build_primary_report(replicas: &[ReplicaRow]) -> StatReport {
    let columns = ["client_addr", "state", "lag_bytes", "replay_lag_s"]
        .into_iter()
        .map(String::from)
        .collect();
    let rows = replicas
        .iter()
        .map(|r| {
            vec![
                r.client_addr.clone().unwrap_or_else(|| "-".to_string()),
                r.state.clone(),
                r.lag_bytes
                    .map_or_else(|| "-".to_string(), |v| v.to_string()),
                r.replay_lag_s
                    .map_or_else(|| "-".to_string(), |v| format!("{v:.1}")),
            ]
        })
        .collect();
    StatReport { columns, rows }
}

fn build_standby_report(last_replay: Option<&str>, lag_s: Option<f64>) -> StatReport {
    StatReport {
        columns: vec!["field".to_string(), "value".to_string()],
        rows: vec![
            vec!["role".to_string(), "standby".to_string()],
            vec![
                "last_replay_timestamp".to_string(),
                last_replay.unwrap_or("-").to_string(),
            ],
            vec![
                "replay_lag_s".to_string(),
                lag_s.map_or_else(|| "-".to_string(), |v| format!("{v:.1}")),
            ],
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_report_lists_each_replica() {
        let replicas = vec![ReplicaRow {
            client_addr: Some("10.0.0.5".to_string()),
            state: "streaming".to_string(),
            lag_bytes: Some(1024),
            replay_lag_s: Some(0.5),
        }];
        let report = build_primary_report(&replicas);
        assert_eq!(
            report.columns,
            vec!["client_addr", "state", "lag_bytes", "replay_lag_s"]
        );
        assert_eq!(report.rows[0], vec!["10.0.0.5", "streaming", "1024", "0.5"]);
    }

    #[test]
    fn primary_report_with_no_replicas_has_no_rows() {
        let report = build_primary_report(&[]);
        assert!(report.rows.is_empty());
    }

    #[test]
    fn missing_replica_fields_render_as_placeholder() {
        let replicas = vec![ReplicaRow {
            client_addr: None,
            state: "catchup".to_string(),
            lag_bytes: None,
            replay_lag_s: None,
        }];
        let report = build_primary_report(&replicas);
        assert_eq!(report.rows[0], vec!["-", "catchup", "-", "-"]);
    }

    #[test]
    fn standby_report_shows_role_and_lag() {
        let report = build_standby_report(Some("2026-07-10 10:00:00+00"), Some(1.2));
        assert_eq!(report.rows[0], vec!["role", "standby"]);
        assert_eq!(
            report.rows[1],
            vec!["last_replay_timestamp", "2026-07-10 10:00:00+00"]
        );
        assert_eq!(report.rows[2], vec!["replay_lag_s", "1.2"]);
    }

    #[test]
    fn standby_report_with_no_replay_yet_is_dash() {
        let report = build_standby_report(None, None);
        assert_eq!(report.rows[1], vec!["last_replay_timestamp", "-"]);
        assert_eq!(report.rows[2], vec!["replay_lag_s", "-"]);
    }
}
