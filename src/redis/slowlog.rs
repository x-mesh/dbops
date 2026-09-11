//! `dbops redis slowlog`: `SLOWLOG GET n` (default 10).

use anyhow::Result;

use crate::frame::exit::unix;
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::redis::connect;

const DEFAULT_N: u32 = 10;

pub async fn run(n: Option<u32>, ctx: &Ctx) -> Result<ExitCode> {
    let n = n.unwrap_or(DEFAULT_N);

    let mut conn = match connect::connect(ctx).await {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let raw = match connect::call::<redis::Value>(
        ctx,
        redis::cmd("SLOWLOG")
            .arg("GET")
            .arg(n)
            .query_async(&mut conn),
    )
    .await
    {
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

/// Pure: a `SLOWLOG GET` reply -> a `StatReport`.
///
/// Each entry is `[id, timestamp, duration_us, [args...], client_addr?,
/// client_name?]` per the `SLOWLOG GET` reply schema; the last two fields
/// were added in Redis 4.0 and render as `"-"` on older servers/entries
/// that omit them.
fn build_report(value: &redis::Value) -> StatReport {
    let columns = [
        "id",
        "timestamp",
        "duration_us",
        "command",
        "client_addr",
        "client_name",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let entries = value.as_sequence().unwrap_or(&[]);
    let rows = entries
        .iter()
        .filter_map(|entry| entry.as_sequence())
        .map(|fields| {
            let get = |i: usize| {
                fields
                    .get(i)
                    .map(value_to_string)
                    .unwrap_or_else(|| "-".to_string())
            };
            let command = fields
                .get(3)
                .and_then(|v| v.as_sequence())
                .map(|args| {
                    args.iter()
                        .map(value_to_string)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_else(|| "-".to_string());
            vec![get(0), get(1), get(2), command, get(4), get(5)]
        })
        .collect();

    StatReport { columns, rows }
}

fn value_to_string(v: &redis::Value) -> String {
    match v {
        redis::Value::Int(n) => n.to_string(),
        redis::Value::BulkString(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        redis::Value::SimpleString(s) => s.clone(),
        redis::Value::Double(d) => d.to_string(),
        redis::Value::Nil => "-".to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::Value;

    fn full_entry(id: i64, ts: i64, dur: i64, args: &[&str], addr: &str, name: &str) -> Value {
        Value::Array(vec![
            Value::Int(id),
            Value::Int(ts),
            Value::Int(dur),
            Value::Array(
                args.iter()
                    .map(|a| Value::BulkString(a.as_bytes().to_vec()))
                    .collect(),
            ),
            Value::BulkString(addr.as_bytes().to_vec()),
            Value::BulkString(name.as_bytes().to_vec()),
        ])
    }

    #[test]
    fn parses_full_six_field_entries() {
        let value = Value::Array(vec![full_entry(
            1,
            1_600_000_000,
            1234,
            &["GET", "foo"],
            "127.0.0.1:1234",
            "",
        )]);
        let report = build_report(&value);
        assert_eq!(
            report.rows[0],
            vec!["1", "1600000000", "1234", "GET foo", "127.0.0.1:1234", ""]
        );
    }

    #[test]
    fn parses_legacy_four_field_entries() {
        let value = Value::Array(vec![Value::Array(vec![
            Value::Int(2),
            Value::Int(1_600_000_001),
            Value::Int(50),
            Value::Array(vec![Value::BulkString(b"PING".to_vec())]),
        ])]);
        let report = build_report(&value);
        assert_eq!(
            report.rows[0],
            vec!["2", "1600000001", "50", "PING", "-", "-"]
        );
    }

    #[test]
    fn empty_slowlog_yields_no_rows() {
        let report = build_report(&Value::Array(vec![]));
        assert!(report.rows.is_empty());
    }

    /// The `SLOWLOG GET` reply's 3rd field is the command's execution time
    /// in *microseconds* (Redis docs), not milliseconds: the column name
    /// must say so explicitly, in both the table header and `--json`
    /// (`StatReport.columns` is the single source for both), so a reader
    /// never has to guess the unit or assume it matches `pg`/`redis health`'s
    /// millisecond-based duration fields elsewhere in this toolkit.
    #[test]
    fn duration_column_states_its_unit_explicitly() {
        let report = build_report(&Value::Array(vec![]));
        assert_eq!(report.columns[2], "duration_us");
        assert!(
            !report.columns.iter().any(|c| c == "duration"),
            "bare 'duration' column would leave the unit ambiguous"
        );
    }
}
