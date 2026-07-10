//! Pure parsing/judging logic for a `replSetGetStatus` response, kept free
//! of any `mongodb::Client`/network dependency so the PRIMARY-election
//! decision table can be unit-tested against mock documents instead of a
//! live replica set. [`health`](crate::mongo::health) and
//! [`replset`](crate::mongo::replset) both build on this.

use anyhow::{Context, Result};
use mongodb::bson::{DateTime, Document};

use crate::frame::result::CheckStatus;

/// One member entry from `replSetGetStatus.members`, reduced to the fields
/// `health`/`replset` care about.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberInfo {
    pub name: String,
    pub state: String,
    /// `1.0` is healthy; anything else (including members the primary can't
    /// currently reach) is not.
    pub health: f64,
    /// Seconds behind the PRIMARY's `optimeDate`. `None` when the member's
    /// optime is unknown (e.g. unreachable) or there is no PRIMARY to
    /// compare against. Always `Some(0)` for the PRIMARY itself.
    pub lag_secs: Option<i64>,
    pub last_heartbeat: Option<DateTime>,
}

/// Parse the `members` array out of a raw `replSetGetStatus` response.
pub fn parse_members(doc: &Document) -> Result<Vec<MemberInfo>> {
    let members_bson = doc
        .get_array("members")
        .context("replSetGetStatus response is missing a 'members' array")?;

    let primary_optime = members_bson.iter().find_map(|entry| {
        let member = entry.as_document()?;
        if member.get_str("stateStr").ok()? != "PRIMARY" {
            return None;
        }
        member.get_datetime("optimeDate").ok().copied()
    });

    let mut members = Vec::with_capacity(members_bson.len());
    for entry in members_bson {
        let member = entry
            .as_document()
            .context("replSetGetStatus 'members' entry is not a document")?;
        let name = member
            .get_str("name")
            .context("replset member is missing 'name'")?
            .to_string();
        let state = member
            .get_str("stateStr")
            .context("replset member is missing 'stateStr'")?
            .to_string();
        // Modern servers report `health` as a double; tolerate an int too
        // rather than failing the whole parse over a wire-format quirk.
        let health = member
            .get_f64("health")
            .or_else(|_| member.get_i32("health").map(f64::from))
            .unwrap_or(0.0);
        let optime = member.get_datetime("optimeDate").ok().copied();
        let last_heartbeat = member.get_datetime("lastHeartbeat").ok().copied();

        let lag_secs = if state == "PRIMARY" {
            Some(0)
        } else {
            match (primary_optime, optime) {
                (Some(primary_dt), Some(member_dt)) => {
                    Some(primary_dt.saturating_duration_since(member_dt).as_secs() as i64)
                }
                _ => None,
            }
        };

        members.push(MemberInfo { name, state, health, lag_secs, last_heartbeat });
    }

    Ok(members)
}

/// Outcome of judging a parsed member list against the health decision
/// table (PRD edge cases) plus optional lag thresholds.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgement {
    pub status: CheckStatus,
    pub summary: String,
    pub max_lag_secs: Option<i64>,
    pub unhealthy_count: usize,
}

/// Judge a parsed member list:
/// - no PRIMARY -> CRITICAL
/// - PRIMARY present but at least one member unhealthy -> WARNING
/// - PRIMARY present, all healthy -> OK
/// - independently, max replication lag crossing `warning_secs`/`critical_secs`
///   raises the status to at least that level (never lowers it).
pub fn judge_replset(members: &[MemberInfo], warning_secs: Option<i64>, critical_secs: Option<i64>) -> Judgement {
    let primary = members.iter().find(|m| m.state == "PRIMARY");
    let unhealthy: Vec<&MemberInfo> = members.iter().filter(|m| m.health < 1.0).collect();
    let max_lag = members.iter().filter_map(|m| m.lag_secs).max();

    let mut status = CheckStatus::Ok;
    if primary.is_none() {
        status = worse(status, CheckStatus::Critical);
    } else if !unhealthy.is_empty() {
        status = worse(status, CheckStatus::Warning);
    }
    if let Some(lag) = max_lag {
        if critical_secs.is_some_and(|c| lag >= c) {
            status = worse(status, CheckStatus::Critical);
        } else if warning_secs.is_some_and(|w| lag >= w) {
            status = worse(status, CheckStatus::Warning);
        }
    }

    let summary = match primary {
        None => format!("no PRIMARY found among {} member(s)", members.len()),
        Some(p) => {
            let mut parts = vec![format!("PRIMARY={}", p.name)];
            if !unhealthy.is_empty() {
                let names: Vec<&str> = unhealthy.iter().map(|m| m.name.as_str()).collect();
                parts.push(format!("unhealthy=[{}]", names.join(",")));
            }
            if let Some(lag) = max_lag {
                parts.push(format!("max_lag={lag}s"));
            }
            parts.join(" ")
        }
    };

    Judgement { status, summary, max_lag_secs: max_lag, unhealthy_count: unhealthy.len() }
}

fn severity_rank(status: CheckStatus) -> u8 {
    match status {
        CheckStatus::Ok => 0,
        CheckStatus::Warning => 1,
        CheckStatus::Critical => 2,
        CheckStatus::Unknown => 3,
    }
}

fn worse(a: CheckStatus, b: CheckStatus) -> CheckStatus {
    if severity_rank(b) > severity_rank(a) {
        b
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use mongodb::bson::doc;

    use super::*;

    // 60s apart, used to build a deterministic lag between PRIMARY and a
    // lagging SECONDARY in the mock documents below.
    const PRIMARY_OPTIME_MS: i64 = 1_700_000_060_000;
    const SECONDARY_OPTIME_MS: i64 = 1_700_000_000_000;

    fn mock_status_doc() -> Document {
        doc! {
            "ok": 1.0,
            "set": "rs0",
            "members": [
                {
                    "name": "mongo-1:27017",
                    "stateStr": "PRIMARY",
                    "health": 1.0,
                    "optimeDate": DateTime::from_millis(PRIMARY_OPTIME_MS),
                    "lastHeartbeat": DateTime::from_millis(PRIMARY_OPTIME_MS),
                },
                {
                    "name": "mongo-2:27017",
                    "stateStr": "SECONDARY",
                    "health": 1.0,
                    "optimeDate": DateTime::from_millis(SECONDARY_OPTIME_MS),
                    "lastHeartbeat": DateTime::from_millis(SECONDARY_OPTIME_MS),
                },
            ],
        }
    }

    #[test]
    fn parse_members_reads_name_state_health_and_lag() {
        let members = parse_members(&mock_status_doc()).unwrap();
        assert_eq!(members.len(), 2);

        let primary = members.iter().find(|m| m.state == "PRIMARY").unwrap();
        assert_eq!(primary.name, "mongo-1:27017");
        assert_eq!(primary.lag_secs, Some(0));

        let secondary = members.iter().find(|m| m.state == "SECONDARY").unwrap();
        assert_eq!(secondary.lag_secs, Some(60));
    }

    #[test]
    fn parse_members_requires_members_array() {
        let doc = doc! { "ok": 1.0 };
        let err = parse_members(&doc).unwrap_err();
        assert!(err.to_string().contains("members"));
    }

    // --- judgement: PRD edge cases -----------------------------------------

    #[test]
    fn no_primary_is_critical() {
        let doc = doc! {
            "members": [
                {
                    "name": "mongo-1:27017",
                    "stateStr": "SECONDARY",
                    "health": 1.0,
                    "optimeDate": DateTime::from_millis(SECONDARY_OPTIME_MS),
                },
                {
                    "name": "mongo-2:27017",
                    "stateStr": "SECONDARY",
                    "health": 1.0,
                    "optimeDate": DateTime::from_millis(SECONDARY_OPTIME_MS),
                },
            ],
        };
        let members = parse_members(&doc).unwrap();
        let judgement = judge_replset(&members, None, None);
        assert_eq!(judgement.status, CheckStatus::Critical);
        assert!(judgement.summary.contains("no PRIMARY"));
    }

    #[test]
    fn one_member_down_with_primary_present_is_warning() {
        let doc = doc! {
            "members": [
                {
                    "name": "mongo-1:27017",
                    "stateStr": "PRIMARY",
                    "health": 1.0,
                    "optimeDate": DateTime::from_millis(PRIMARY_OPTIME_MS),
                },
                {
                    "name": "mongo-2:27017",
                    "stateStr": "(not reachable/healthy)",
                    "health": 0.0,
                },
            ],
        };
        let members = parse_members(&doc).unwrap();
        let judgement = judge_replset(&members, None, None);
        assert_eq!(judgement.status, CheckStatus::Warning);
        assert_eq!(judgement.unhealthy_count, 1);
        assert!(judgement.summary.contains("mongo-2:27017"));
    }

    #[test]
    fn all_healthy_with_primary_is_ok() {
        let members = parse_members(&mock_status_doc()).unwrap();
        let judgement = judge_replset(&members, None, None);
        assert_eq!(judgement.status, CheckStatus::Ok);
    }

    #[test]
    fn lag_past_warning_threshold_raises_status() {
        let members = parse_members(&mock_status_doc()).unwrap();
        let judgement = judge_replset(&members, Some(30), None);
        assert_eq!(judgement.status, CheckStatus::Warning);
        assert_eq!(judgement.max_lag_secs, Some(60));
    }

    #[test]
    fn lag_past_critical_threshold_wins_over_warning() {
        let members = parse_members(&mock_status_doc()).unwrap();
        let judgement = judge_replset(&members, Some(10), Some(30));
        assert_eq!(judgement.status, CheckStatus::Critical);
    }

    #[test]
    fn unhealthy_member_plus_critical_lag_stays_critical() {
        let doc = doc! {
            "members": [
                {
                    "name": "mongo-1:27017",
                    "stateStr": "PRIMARY",
                    "health": 1.0,
                    "optimeDate": DateTime::from_millis(PRIMARY_OPTIME_MS),
                },
                {
                    "name": "mongo-2:27017",
                    "stateStr": "SECONDARY",
                    "health": 0.0,
                    "optimeDate": DateTime::from_millis(SECONDARY_OPTIME_MS),
                },
            ],
        };
        let members = parse_members(&doc).unwrap();
        // unhealthy member alone -> Warning, but lag also breaches critical.
        let judgement = judge_replset(&members, Some(10), Some(30));
        assert_eq!(judgement.status, CheckStatus::Critical);
    }
}
