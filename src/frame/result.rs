use serde::Serialize;

/// Nagios-style check outcome. The four-way vocabulary (not a boolean or a
/// free-form string) is what lets [`crate::frame::exit`] map every check in
/// the toolkit onto the same plugin exit-code contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[allow(dead_code)] // consumed by domain modules once they stop being stubs
pub enum CheckStatus {
    Ok,
    Warning,
    Critical,
    Unknown,
}

impl std::fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            CheckStatus::Ok => "OK",
            CheckStatus::Warning => "WARNING",
            CheckStatus::Critical => "CRITICAL",
            CheckStatus::Unknown => "UNKNOWN",
        };
        f.write_str(text)
    }
}

/// One perfdata point, rendered as `name=value<unit>;<warn>;<crit>` in text
/// mode (the check_postgres/nagios perfdata convention). `warn`/`crit` are
/// strings rather than numbers because nagios threshold ranges can be more
/// than a single value (e.g. `"80:90"`, `"~:80"`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[allow(dead_code)] // consumed by domain modules once they stop being stubs
pub struct Metric {
    pub name: String,
    pub value: f64,
    pub unit: Option<String>,
    pub warn: Option<String>,
    pub crit: Option<String>,
}

/// Outcome of a single check subcommand (e.g. `dbops pg health`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[allow(dead_code)] // consumed by domain modules once they stop being stubs
pub struct CheckResult {
    pub status: CheckStatus,
    pub summary: String,
    pub metrics: Vec<Metric>,
}

/// Tabular output of a stat/listing subcommand (e.g. `dbops os indices`).
/// Every row must have the same length as `columns`; renderers do not
/// validate this, so callers own that invariant.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[allow(dead_code)] // consumed by domain modules once they stop being stubs
pub struct StatReport {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_status_serializes_to_nagios_vocabulary() {
        assert_eq!(serde_json::to_string(&CheckStatus::Ok).unwrap(), "\"OK\"");
        assert_eq!(serde_json::to_string(&CheckStatus::Warning).unwrap(), "\"WARNING\"");
        assert_eq!(serde_json::to_string(&CheckStatus::Critical).unwrap(), "\"CRITICAL\"");
        assert_eq!(serde_json::to_string(&CheckStatus::Unknown).unwrap(), "\"UNKNOWN\"");
    }

    #[test]
    fn display_matches_serialized_text() {
        for status in [
            CheckStatus::Ok,
            CheckStatus::Warning,
            CheckStatus::Critical,
            CheckStatus::Unknown,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{status}\""));
        }
    }
}
