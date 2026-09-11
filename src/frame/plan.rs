//! Preview of the create/drop/insert/... actions a destructive command
//! intends to run, before it runs them. [`crate::frame::guard::authorize`]
//! is the only module that decides *whether* a plan gets applied; this one
//! only knows how to describe *what* it would do, and renders that preview
//! identically for `--dry-run` and for the confirmation prompt a real run
//! shows first: one plan, two consumers, never two descriptions of the
//! same operation drifting apart.

// Not wired into any domain module yet -- that lands with the M3
// destructive-command tasks (`init`/`reset`/`seed`), which build a
// `PlanPreview` and hand it to `guard::authorize`. Until then this is only
// reachable from this module's own tests. Same reasoning as
// `config`/`secret`'s `#![allow(dead_code)]`.
#![allow(dead_code)]

use serde::Serialize;

/// Category of a single planned action. `Other` is an escape hatch for
/// domain-specific operations (e.g. an OpenSearch reindex) that don't fit
/// the common set below, so this enum doesn't have to anticipate every
/// domain module's vocabulary up front.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Create,
    Drop,
    Truncate,
    Insert,
    Update,
    Delete,
    Grant,
    Index,
    Other(String),
}

impl ActionKind {
    /// Terraform-`plan`-style leading symbol: `+` additive, `-`
    /// destructive/data-losing, `~` in-place mutation.
    fn symbol(&self) -> &'static str {
        match self {
            ActionKind::Create | ActionKind::Insert => "+",
            ActionKind::Drop | ActionKind::Delete | ActionKind::Truncate => "-",
            ActionKind::Update | ActionKind::Grant | ActionKind::Index | ActionKind::Other(_) => {
                "~"
            }
        }
    }

    fn label(&self) -> &str {
        match self {
            ActionKind::Create => "create",
            ActionKind::Drop => "drop",
            ActionKind::Truncate => "truncate",
            ActionKind::Insert => "insert",
            ActionKind::Update => "update",
            ActionKind::Delete => "delete",
            ActionKind::Grant => "grant",
            ActionKind::Index => "index",
            ActionKind::Other(label) => label,
        }
    }
}

/// One action a destructive command intends to perform, e.g. "drop table
/// orders_staging" or "insert ~1,200 seed rows into sessions".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlannedAction {
    pub kind: ActionKind,
    pub target: String,
    pub detail: String,
    pub estimated_records: Option<u64>,
}

/// Full preview of a destructive command's intended effects.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanPreview {
    pub actions: Vec<PlannedAction>,
}

impl PlanPreview {
    /// The single name a `--confirm-name` flag must match to authorize this
    /// plan against a protected profile.
    ///
    /// `None` when the plan is empty or touches more than one distinct
    /// target. An ambiguous plan can never be name-confirmed, so
    /// [`crate::frame::guard::authorize`] always declines it for protected
    /// profiles rather than guess which target the operator meant.
    pub fn confirm_target(&self) -> Option<&str> {
        let mut targets = self.actions.iter().map(|a| a.target.as_str());
        let first = targets.next()?;
        targets.all(|t| t == first).then_some(first)
    }

    /// Render as terraform-`plan`-style text, or as JSON when `json` is
    /// true. Mirrors the single-rendering-path convention in
    /// `frame::output` (`render_check`/`render_stat`): whatever the caller
    /// shows a user always goes through here, never a one-off `println!`.
    pub fn render(&self, json: bool) -> String {
        if json {
            return serde_json::to_string_pretty(self)
                .expect("PlanPreview contains only JSON-safe types");
        }
        self.render_text()
    }

    fn render_text(&self) -> String {
        if self.actions.is_empty() {
            return "no actions planned".to_string();
        }

        let mut lines: Vec<String> = self
            .actions
            .iter()
            .map(|action| {
                let mut line = format!(
                    "  {} {:<8} {}",
                    action.kind.symbol(),
                    action.kind.label(),
                    action.target
                );
                if !action.detail.is_empty() {
                    line.push_str(&format!("  ({})", action.detail));
                }
                if let Some(n) = action.estimated_records {
                    line.push_str(&format!(
                        " \u{2014} ~{n} record{}",
                        if n == 1 { "" } else { "s" }
                    ));
                }
                line
            })
            .collect();

        lines.push(String::new());
        lines.push(format!(
            "Plan: {} action{}.",
            self.actions.len(),
            if self.actions.len() == 1 { "" } else { "s" }
        ));
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(kind: ActionKind, target: &str) -> PlannedAction {
        PlannedAction {
            kind,
            target: target.to_string(),
            detail: String::new(),
            estimated_records: None,
        }
    }

    // --- confirm_target ------------------------------------------------------

    #[test]
    fn confirm_target_is_none_when_empty() {
        let plan = PlanPreview { actions: vec![] };
        assert_eq!(plan.confirm_target(), None);
    }

    #[test]
    fn confirm_target_is_some_when_every_action_shares_a_target() {
        let plan = PlanPreview {
            actions: vec![
                action(ActionKind::Drop, "orders"),
                action(ActionKind::Truncate, "orders"),
            ],
        };
        assert_eq!(plan.confirm_target(), Some("orders"));
    }

    #[test]
    fn confirm_target_is_none_when_targets_diverge() {
        let plan = PlanPreview {
            actions: vec![
                action(ActionKind::Drop, "orders"),
                action(ActionKind::Drop, "sessions"),
            ],
        };
        assert_eq!(plan.confirm_target(), None);
    }

    // --- rendering -------------------------------------------------------------

    #[test]
    fn json_render_round_trips_through_serde() {
        let plan = PlanPreview {
            actions: vec![PlannedAction {
                kind: ActionKind::Insert,
                target: "sessions".to_string(),
                detail: "seed data".to_string(),
                estimated_records: Some(500),
            }],
        };
        let out = plan.render(true);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(value["actions"][0]["kind"], "insert");
        assert_eq!(value["actions"][0]["target"], "sessions");
        assert_eq!(value["actions"][0]["estimated_records"], 500);
    }

    #[test]
    fn text_render_uses_terraform_style_symbols_and_summary() {
        let plan = PlanPreview {
            actions: vec![
                action(ActionKind::Create, "index-a"),
                action(ActionKind::Drop, "index-b"),
            ],
        };
        let out = plan.render(false);
        assert!(out.contains("+ create"));
        assert!(out.contains("- drop"));
        assert!(out.contains("Plan: 2 actions."));
    }

    #[test]
    fn empty_plan_renders_a_clear_no_op_message() {
        let plan = PlanPreview { actions: vec![] };
        assert_eq!(plan.render(false), "no actions planned");
    }
}
