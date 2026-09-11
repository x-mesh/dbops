//! `dbops redis stats`: used_memory, maxmemory, mem_fragmentation_ratio,
//! evicted_keys.
//!
//! `evicted_keys` lives in `INFO`'s `# Stats` section, not `# Memory`.
//! Confirmed against a live `redis:7-alpine` server, `INFO memory` alone
//! never has it. Requesting the default (no-section) `INFO` reply instead
//! covers both sections in one round trip.

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

    let raw = match connect::call::<String>(ctx, redis::cmd("INFO").query_async(&mut conn)).await {
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

/// Pure: `INFO` text -> the four PRD metrics as a `StatReport`. Missing
/// fields render as `"-"` rather than being dropped, so `--json` output
/// always has the same shape regardless of server version.
fn build_report(raw: &str) -> StatReport {
    let fields = info::parse_info(raw);
    let get = |key: &str| fields.get(key).cloned().unwrap_or_else(|| "-".to_string());
    StatReport {
        columns: vec!["metric".to_string(), "value".to_string()],
        rows: vec![
            vec!["used_memory".to_string(), get("used_memory")],
            vec!["maxmemory".to_string(), get("maxmemory")],
            vec![
                "mem_fragmentation_ratio".to_string(),
                get("mem_fragmentation_ratio"),
            ],
            vec!["evicted_keys".to_string(), get("evicted_keys")],
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors real `INFO` (no section arg) shape: evicted_keys lives under
    // `# Stats`, not `# Memory`: the fixture must reflect that split so a
    // regression back to `INFO memory`-only can't silently pass.
    const FIXTURE: &str = "\
# Memory
used_memory:1048576
used_memory_human:1.00M
maxmemory:0
maxmemory_policy:noeviction
mem_fragmentation_ratio:1.05

# Stats
evicted_keys:3
";

    #[test]
    fn builds_the_four_prd_metrics() {
        let report = build_report(FIXTURE);
        assert_eq!(report.columns, vec!["metric", "value"]);
        assert_eq!(report.rows[0], vec!["used_memory", "1048576"]);
        assert_eq!(report.rows[1], vec!["maxmemory", "0"]);
        assert_eq!(report.rows[2], vec!["mem_fragmentation_ratio", "1.05"]);
        assert_eq!(report.rows[3], vec!["evicted_keys", "3"]);
    }

    #[test]
    fn missing_fields_render_as_placeholder() {
        let report = build_report("# Memory\nused_memory:100\n");
        assert_eq!(report.rows[1], vec!["maxmemory", "-"]);
        assert_eq!(report.rows[2], vec!["mem_fragmentation_ratio", "-"]);
        assert_eq!(report.rows[3], vec!["evicted_keys", "-"]);
    }
}
