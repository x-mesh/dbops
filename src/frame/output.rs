//! The single rendering path every subcommand funnels through: either a
//! nagios-style one-line check result ([`render_check`]) or a tabular stat
//! listing ([`render_stat`]). Both honor the global `--json` flag and both
//! adapt table styling to whether stdout is a real terminal or a pipe.

use std::io::IsTerminal;

use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::{presets, Attribute, Cell, Color, ContentArrangement, Table};

use crate::frame::result::{CheckResult, CheckStatus, Metric, StatReport};

/// Table rows beyond this count are summarized instead of printed in full.
/// `--json` output is never truncated.
const MAX_TABLE_ROWS: usize = 100;

/// Render a single check outcome.
///
/// `domain`/`command` (e.g. `"pg"`/`"health"`) are not part of [`CheckResult`]
/// itself. That type stays domain-agnostic so it serializes identically
/// across every check in the toolkit, but the nagios text line needs them
/// as its `<DOMAIN> <COMMAND> ...` prefix, so callers pass them in here.
#[allow(dead_code)] // consumed by domain modules once they stop being stubs
pub fn render_check(domain: &str, command: &str, result: &CheckResult, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(result)
            .expect("CheckResult contains only JSON-safe types");
    }
    render_check_text(domain, command, result, std::io::stdout().is_terminal())
}

fn render_check_text(domain: &str, command: &str, result: &CheckResult, tty: bool) -> String {
    let status_text = colorize(result.status, &result.status.to_string(), tty);
    let mut line = format!(
        "{} {} {}: {}",
        domain.to_uppercase(),
        command.to_uppercase(),
        status_text,
        escape_control_chars(&result.summary),
    );

    if !result.metrics.is_empty() {
        let perfdata = result
            .metrics
            .iter()
            .map(render_metric)
            .collect::<Vec<_>>()
            .join(" ");
        line.push_str(" | ");
        line.push_str(&perfdata);
    }

    line
}

/// `name=value<unit>;<warn>;<crit>`: the check_postgres/nagios perfdata
/// convention. Missing unit/warn/crit render as an empty field, not `"0"`
/// or `"None"`, matching what nagios plugins emit.
fn render_metric(m: &Metric) -> String {
    format!(
        "{}={}{};{};{}",
        escape_control_chars(&m.name),
        m.value,
        m.unit.as_deref().unwrap_or(""),
        m.warn.as_deref().unwrap_or(""),
        m.crit.as_deref().unwrap_or(""),
    )
}

fn colorize(status: CheckStatus, text: &str, tty: bool) -> String {
    if !tty {
        return text.to_string();
    }
    let ansi_code = match status {
        CheckStatus::Ok => "32",       // green
        CheckStatus::Warning => "33",  // yellow
        CheckStatus::Critical => "31", // red
        CheckStatus::Unknown => "35",  // magenta
    };
    format!("\x1b[{ansi_code}m{text}\x1b[0m")
}

/// Render a stat/listing report as a table (or `--json`).
#[allow(dead_code)] // consumed by domain modules once they stop being stubs
pub fn render_stat(report: &StatReport, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(report)
            .expect("StatReport contains only JSON-safe types");
    }
    render_stat_table(report, std::io::stdout().is_terminal())
}

fn render_stat_table(report: &StatReport, tty: bool) -> String {
    let mut table = Table::new();
    // Box-drawing choice is ours to make (ASCII stays grep/pipe-safe);
    // comfy-table separately strips fg/attribute styling on its own
    // std::io::IsTerminal check, so the Cell colors below are automatically
    // dropped for non-tty output without any extra branching here.
    table.load_preset(if tty {
        presets::UTF8_FULL
    } else {
        presets::ASCII_FULL
    });
    if tty {
        table.apply_modifier(UTF8_ROUND_CORNERS);
    }
    table.set_content_arrangement(ContentArrangement::Dynamic);

    let header: Vec<Cell> = report
        .columns
        .iter()
        .map(|c| {
            Cell::new(escape_control_chars(c))
                .add_attribute(Attribute::Bold)
                .fg(Color::Cyan)
        })
        .collect();
    table.set_header(header);

    let total = report.rows.len();
    let shown = total.min(MAX_TABLE_ROWS);
    for row in &report.rows[..shown] {
        let cells: Vec<String> = row.iter().map(|v| escape_control_chars(v)).collect();
        table.add_row(cells);
    }

    let mut out = table.to_string();
    if total > MAX_TABLE_ROWS {
        out.push('\n');
        out.push_str(&format!(
            "… {} more rows (use --json for full output)",
            total - MAX_TABLE_ROWS
        ));
    }
    out
}

/// Escape newlines/tabs/other control characters so a value can never break
/// a nagios text line or split a table row across lines. `--json` output
/// bypasses this entirely: serde_json already encodes control characters
/// correctly for JSON strings.
fn escape_control_chars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::result::{CheckResult, CheckStatus, Metric, StatReport};

    fn sample_check(status: CheckStatus, summary: &str) -> CheckResult {
        CheckResult {
            status,
            summary: summary.to_string(),
            metrics: vec![Metric {
                name: "disk_pct".to_string(),
                value: 85.0,
                unit: Some("%".to_string()),
                warn: Some("80".to_string()),
                crit: Some("90".to_string()),
            }],
        }
    }

    #[test]
    fn json_check_output_matches_schema_and_parses() {
        let result = sample_check(CheckStatus::Warning, "disk usage high");
        let out = render_check("pg", "diskcheck", &result, true);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(value["status"], "WARNING");
        assert_eq!(value["summary"], "disk usage high");
        assert_eq!(value["metrics"][0]["name"], "disk_pct");
        assert_eq!(value["metrics"][0]["value"], 85.0);
    }

    #[test]
    fn text_check_output_has_nagios_perfdata_line() {
        let result = sample_check(CheckStatus::Critical, "disk usage critical");
        let out = render_check("pg", "diskcheck", &result, false);
        assert!(out.starts_with("PG DISKCHECK CRITICAL: disk usage critical"));
        assert!(out.contains("disk_pct=85%;80;90"));
    }

    #[test]
    fn control_characters_are_escaped_in_check_text() {
        let result = CheckResult {
            status: CheckStatus::Ok,
            summary: "line1\nline2\ttabbed".to_string(),
            metrics: vec![],
        };
        let out = render_check("pg", "check", &result, false);
        assert!(out.contains("line1\\nline2\\ttabbed"));
        assert_eq!(
            out.matches('\n').count(),
            0,
            "escaped output must stay one line"
        );
    }

    #[test]
    fn control_characters_are_escaped_in_table_cells() {
        let report = StatReport {
            columns: vec!["col".to_string()],
            rows: vec![vec!["a\nb\tc".to_string()]],
        };
        let out = render_stat(&report, false);
        assert!(out.contains("a\\nb\\tc"));
    }

    #[test]
    fn stat_table_truncates_past_the_row_limit() {
        let rows: Vec<Vec<String>> = (0..101).map(|i| vec![i.to_string()]).collect();
        let report = StatReport {
            columns: vec!["n".to_string()],
            rows,
        };
        let out = render_stat(&report, false);
        assert!(out.contains("1 more rows (use --json for full output)"));
    }

    #[test]
    fn stat_json_is_never_truncated() {
        let rows: Vec<Vec<String>> = (0..101).map(|i| vec![i.to_string()]).collect();
        let report = StatReport {
            columns: vec!["n".to_string()],
            rows,
        };
        let out = render_stat(&report, true);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(value["rows"].as_array().unwrap().len(), 101);
    }

    #[test]
    fn stat_table_under_the_limit_has_no_truncation_note() {
        let report = StatReport {
            columns: vec!["n".to_string()],
            rows: vec![vec!["1".to_string()], vec!["2".to_string()]],
        };
        let out = render_stat(&report, false);
        assert!(!out.contains("more rows"));
    }
}
