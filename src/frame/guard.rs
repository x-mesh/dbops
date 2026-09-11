//! The single gate every destructive command (M3's `init`/`reset`/`seed`
//! subcommands) must pass through before mutating anything.
//!
//! `--dry-run` and a real run share the exact same [`PlanPreview`] — a
//! caller builds one plan, hands it to [`authorize`], and only calls its
//! own `apply()` when the result is [`GuardDecision::Proceed`]. A
//! [`GuardDecision::Declined`] must never be followed by any side effect.

// Not called from any domain module yet -- that lands with the M3
// destructive-command tasks. Until then this is only reachable from this
// module's own tests. Same reasoning as `plan`'s `#![allow(dead_code)]`.
#![allow(dead_code)]

use std::io::IsTerminal;

use anyhow::{Context, Result};
use dialoguer::{Confirm, Input};

use crate::frame::ctx::Ctx;
use crate::frame::plan::PlanPreview;

/// Outcome of [`authorize`]. The caller — never this module — decides what
/// to do next: `Proceed` calls `apply()`; `DryRun` and `Declined` both
/// return without ever calling it (`DryRun` maps to exit 0, `Declined` to
/// [`crate::frame::exit::unix::CONFIRMATION_DECLINED`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardDecision {
    Proceed,
    DryRun,
    Declined,
}

/// Decide whether a destructive plan may run.
///
/// `confirm_name` is the destructive subcommand's local `--confirm-name`
/// flag value — there is no global flag for this (the global `Cli` is
/// frozen), so every M3 subcommand that can touch a protected profile adds
/// its own and threads it through here. It's only consulted when
/// [`Ctx::profile`] is
/// [`protected`](crate::frame::config::ResolvedProfile::protected).
///
/// Rule order (first match wins):
/// 1. `ctx.dry_run` — render the plan, return `DryRun` without asking anything.
/// 2. Non-interactive (`stdin` is not a TTY) and `!ctx.yes` — refuse outright.
/// 3. Protected profile — `confirm_name` must equal
///    [`PlanPreview::confirm_target`]. Non-interactively, any mismatch (or
///    missing flag) is a hard `Declined`; interactively, the operator gets
///    a chance to type the name at a prompt instead.
/// 4. Interactive and `!ctx.yes` — one last "really do this?" prompt.
/// 5. Otherwise — `Proceed`.
pub fn authorize(
    ctx: &Ctx,
    plan: &PlanPreview,
    confirm_name: Option<&str>,
) -> Result<GuardDecision> {
    if ctx.dry_run {
        println!("{}", plan.render(ctx.json));
        return Ok(GuardDecision::DryRun);
    }

    let is_tty = std::io::stdin().is_terminal();
    let expected_name = plan.confirm_target();
    let mut confirm_name = confirm_name.map(str::to_string);
    // Local override, not a write-back to `ctx`: once the operator answers
    // the interactive "really do this?" prompt, later loop iterations must
    // stop re-asking it -- exactly what a real `--yes` flag would already
    // have skipped.
    let mut yes = ctx.yes;

    loop {
        match evaluate(
            is_tty,
            yes,
            ctx.profile.protected,
            confirm_name.as_deref(),
            expected_name,
        ) {
            Verdict::Proceed => return Ok(GuardDecision::Proceed),
            Verdict::DeclinedNonInteractive => {
                eprintln!(
                    "error: confirmation required. Re-run with --yes in a non-interactive session, \
                     or run this command from a terminal"
                );
                return Ok(GuardDecision::Declined);
            }
            Verdict::DeclinedNameMismatch => {
                eprintln!(
                    "error: profile '{}' is protected; --confirm-name must exactly match '{}'",
                    ctx.profile.name,
                    expected_name.unwrap_or("<ambiguous plan target>"),
                );
                return Ok(GuardDecision::Declined);
            }
            Verdict::NeedsNameConfirmation => {
                eprintln!(
                    "profile '{}' is protected; type its name to continue",
                    ctx.profile.name
                );
                let typed: String = Input::new()
                    .with_prompt("confirm target name")
                    .interact_text()
                    .context("failed to read confirmation input")?;
                confirm_name = Some(typed);
            }
            Verdict::NeedsFinalConfirmation => {
                let proceed = Confirm::new()
                    .with_prompt(format!(
                        "apply {} action(s)? this cannot be undone",
                        plan.actions.len()
                    ))
                    .default(false)
                    .interact()
                    .context("failed to read confirmation prompt")?;
                if !proceed {
                    return Ok(GuardDecision::Declined);
                }
                yes = true;
            }
        }
    }
}

/// Pure decision core, injected with `is_tty` instead of reading `stdin`
/// directly so the full rule matrix is testable without a real terminal.
/// Has no I/O side effects; [`authorize`] is the only caller and owns every
/// prompt/print the returned step implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Proceed,
    DeclinedNonInteractive,
    DeclinedNameMismatch,
    NeedsNameConfirmation,
    NeedsFinalConfirmation,
}

fn evaluate(
    is_tty: bool,
    yes: bool,
    protected: bool,
    confirm_name: Option<&str>,
    expected_name: Option<&str>,
) -> Verdict {
    if !is_tty && !yes {
        return Verdict::DeclinedNonInteractive;
    }

    let name_matches =
        matches!((confirm_name, expected_name), (Some(given), Some(exp)) if given == exp);

    if protected && !name_matches {
        return if is_tty {
            Verdict::NeedsNameConfirmation
        } else {
            Verdict::DeclinedNameMismatch
        };
    }

    if is_tty && !yes {
        return Verdict::NeedsFinalConfirmation;
    }

    Verdict::Proceed
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::frame::config::ResolvedProfile;
    use crate::frame::plan::{ActionKind, PlannedAction};

    fn test_ctx(dry_run: bool, yes: bool, protected: bool) -> Ctx {
        Ctx {
            profile: ResolvedProfile {
                name: "prod".to_string(),
                protected,
                ..ResolvedProfile::default()
            },
            config: None,
            json: false,
            timeout: Duration::from_secs(5),
            dry_run,
            yes,
            insecure: false,
            verbose: 0,
        }
    }

    fn sample_plan(target: &str) -> PlanPreview {
        PlanPreview {
            actions: vec![PlannedAction {
                kind: ActionKind::Drop,
                target: target.to_string(),
                detail: String::new(),
                estimated_records: None,
            }],
        }
    }

    // --- evaluate(): pure decision matrix, TTY injected as a parameter ------

    #[test]
    fn non_tty_without_yes_is_declined() {
        assert_eq!(
            evaluate(false, false, false, None, None),
            Verdict::DeclinedNonInteractive
        );
    }

    #[test]
    fn non_tty_with_yes_and_unprotected_proceeds() {
        assert_eq!(evaluate(false, true, false, None, None), Verdict::Proceed);
    }

    #[test]
    fn protected_name_mismatch_non_tty_is_declined() {
        assert_eq!(
            evaluate(false, true, true, Some("staging"), Some("prod")),
            Verdict::DeclinedNameMismatch
        );
    }

    #[test]
    fn protected_missing_confirm_name_non_tty_is_declined() {
        assert_eq!(
            evaluate(false, true, true, None, Some("prod")),
            Verdict::DeclinedNameMismatch
        );
    }

    #[test]
    fn protected_name_match_and_yes_proceeds() {
        assert_eq!(
            evaluate(false, true, true, Some("prod"), Some("prod")),
            Verdict::Proceed
        );
        assert_eq!(
            evaluate(true, true, true, Some("prod"), Some("prod")),
            Verdict::Proceed
        );
    }

    #[test]
    fn protected_name_mismatch_tty_asks_to_retype_instead_of_declining() {
        assert_eq!(
            evaluate(true, true, true, None, Some("prod")),
            Verdict::NeedsNameConfirmation
        );
    }

    #[test]
    fn tty_without_yes_and_unprotected_asks_for_final_confirmation() {
        assert_eq!(
            evaluate(true, false, false, None, None),
            Verdict::NeedsFinalConfirmation
        );
    }

    #[test]
    fn tty_with_yes_and_unprotected_proceeds() {
        assert_eq!(evaluate(true, true, false, None, None), Verdict::Proceed);
    }

    #[test]
    fn ambiguous_plan_target_never_satisfies_protected_confirmation() {
        // expected_name = None happens when a plan touches more than one
        // distinct target (see PlanPreview::confirm_target).
        assert_eq!(
            evaluate(false, true, true, Some("prod"), None),
            Verdict::DeclinedNameMismatch
        );
        assert_eq!(
            evaluate(true, true, true, Some("prod"), None),
            Verdict::NeedsNameConfirmation
        );
    }

    // --- authorize(): dry-run short-circuits before any TTY/prompt logic ---

    #[test]
    fn dry_run_returns_without_ever_reaching_apply() {
        let ctx = test_ctx(true, false, false);
        let plan = sample_plan("orders");

        let decision = authorize(&ctx, &plan, None).unwrap();
        assert_eq!(decision, GuardDecision::DryRun);

        // Model the call-site contract (apply() only runs on `Proceed`)
        // instead of trusting the return value blindly -- this is what
        // actually proves dry-run never reaches apply.
        let mut applied = false;
        if decision == GuardDecision::Proceed {
            applied = true;
        }
        assert!(!applied, "dry-run must never trigger apply()");
    }

    #[test]
    fn dry_run_wins_even_over_a_protected_profile() {
        let ctx = test_ctx(true, false, true);
        let plan = sample_plan("prod");
        assert_eq!(authorize(&ctx, &plan, None).unwrap(), GuardDecision::DryRun);
    }
}
