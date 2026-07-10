//! `dbops redis replication`: `INFO replication` -> role, connected slave
//! count, and master/slave offset lag.

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

    let raw =
        match connect::call::<String>(ctx, redis::cmd("INFO").arg("replication").query_async(&mut conn)).await {
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

/// Pure: `INFO replication` text -> a `field`/`value` `StatReport`.
///
/// Rendered fields depend on `role`, since master and replica `INFO`
/// output are shaped differently: a master lists `connected_slaves` plus
/// one row per `slaveN:` line, while a replica reports `master_host` /
/// `master_link_status` plus a computed `offset_lag` between the two
/// sides' replication offsets. A flat key-value shape (rather than fixed
/// columns) is what lets both shapes fit the same `StatReport` type.
fn build_report(raw: &str) -> StatReport {
    let fields = info::parse_info(raw);
    let role = fields.get("role").cloned().unwrap_or_else(|| "unknown".to_string());
    let mut rows = vec![vec!["role".to_string(), role.clone()]];

    if role == "master" {
        rows.extend(master_rows(&fields));
    } else {
        rows.extend(replica_rows(&fields));
    }

    StatReport {
        columns: vec!["field".to_string(), "value".to_string()],
        rows,
    }
}

fn master_rows(fields: &std::collections::HashMap<String, String>) -> Vec<Vec<String>> {
    let connected_slaves = fields.get("connected_slaves").cloned().unwrap_or_else(|| "0".to_string());
    let mut rows = vec![
        vec!["connected_slaves".to_string(), connected_slaves.clone()],
        vec![
            "master_repl_offset".to_string(),
            fields.get("master_repl_offset").cloned().unwrap_or_else(|| "0".to_string()),
        ],
    ];

    let slave_count: u32 = connected_slaves.parse().unwrap_or(0);
    for i in 0..slave_count {
        let key = format!("slave{i}");
        if let Some(raw_fields) = fields.get(&key) {
            let slave = info::parse_fields(raw_fields);
            let get = |field: &str| slave.get(field).cloned().unwrap_or_else(|| "-".to_string());
            let summary = format!(
                "ip={},port={},state={},offset={},lag={}",
                get("ip"),
                get("port"),
                get("state"),
                get("offset"),
                get("lag"),
            );
            rows.push(vec![key, summary]);
        }
    }
    rows
}

fn replica_rows(fields: &std::collections::HashMap<String, String>) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for key in ["master_host", "master_port", "master_link_status", "master_repl_offset", "slave_repl_offset"] {
        if let Some(value) = fields.get(key) {
            rows.push(vec![key.to_string(), value.clone()]);
        }
    }

    if let (Some(master_offset), Some(slave_offset)) = (
        fields.get("master_repl_offset").and_then(|v| v.parse::<i64>().ok()),
        fields.get("slave_repl_offset").and_then(|v| v.parse::<i64>().ok()),
    ) {
        rows.push(vec!["offset_lag".to_string(), (master_offset - slave_offset).abs().to_string()]);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER_FIXTURE: &str = "\
# Replication
role:master
connected_slaves:1
slave0:ip=127.0.0.1,port=6380,state=online,offset=14210,lag=0
master_failover_state:no-failover
master_replid:abc123
master_repl_offset:14210
";

    const REPLICA_FIXTURE: &str = "\
# Replication
role:slave
master_host:127.0.0.1
master_port:6379
master_link_status:up
master_repl_offset:14210
slave_repl_offset:14200
";

    #[test]
    fn master_report_lists_role_and_each_slave() {
        let report = build_report(MASTER_FIXTURE);
        assert_eq!(report.rows[0], vec!["role", "master"]);
        assert_eq!(report.rows[1], vec!["connected_slaves", "1"]);
        assert_eq!(report.rows[2], vec!["master_repl_offset", "14210"]);
        assert_eq!(report.rows[3], vec!["slave0", "ip=127.0.0.1,port=6380,state=online,offset=14210,lag=0"]);
    }

    #[test]
    fn replica_report_computes_offset_lag() {
        let report = build_report(REPLICA_FIXTURE);
        assert_eq!(report.rows[0], vec!["role", "slave"]);
        assert!(report.rows.contains(&vec!["master_link_status".to_string(), "up".to_string()]));
        assert!(report.rows.contains(&vec!["offset_lag".to_string(), "10".to_string()]));
    }

    #[test]
    fn master_with_no_slaves_has_no_slave_rows() {
        let fixture = "# Replication\nrole:master\nconnected_slaves:0\nmaster_repl_offset:0\n";
        let report = build_report(fixture);
        assert_eq!(report.rows.len(), 3);
    }
}
