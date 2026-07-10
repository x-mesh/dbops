//! `dbops mongo init db|user` and `dbops mongo reset db`.
//!
//! MongoDB has no explicit "create database" primitive — a database exists
//! once it holds at least one collection, and disappears once its last
//! collection is dropped. `init db` therefore probes `listDatabaseNames`
//! first: if the name is already present this is a no-op (exit 0,
//! idempotent by construction — no `--if-not-exists` flag needed); otherwise
//! it materializes the database by creating a placeholder `_dbops_init`
//! collection. `reset db` is the mirror image: `dropDatabase` deletes every
//! collection in it, which *is* mongo's version of "wipe and reinitialize" —
//! there is nothing left to recreate afterward, since the next write (or the
//! next `mongo init db`) brings the database back implicitly. Both facts are
//! called out in the commands' own output so an operator used to a
//! create/drop/recreate cycle (e.g. postgres) isn't surprised.
//!
//! Every mutation here is planned first ([`PlanPreview`]) from a live probe
//! of the target — run unconditionally, `--dry-run` or not, so the preview
//! reflects real state — and only applied once [`guard::authorize`] returns
//! [`GuardDecision::Proceed`]; see that module's doc comment for the exact
//! decision table (dry-run / non-interactive / protected-profile rules).

use anyhow::{Context, Result};
use mongodb::bson::{doc, Document};
use mongodb::Client;

use crate::frame::exit::unix;
use crate::frame::guard::{self, GuardDecision};
use crate::frame::plan::{ActionKind, PlanPreview, PlannedAction};
use crate::frame::{Ctx, ExitCode};
use crate::mongo::client;

/// Placeholder collection created by `init db` solely to materialize an
/// otherwise-empty database (mongo databases don't exist until they hold a
/// collection).
const INIT_MARKER_COLLECTION: &str = "_dbops_init";

/// Env var carrying a new user's password so it never has to appear as a
/// plaintext CLI argument (visible via `ps`, shell history, `/proc/*/cmdline`).
const PASSWORD_ENV_VAR: &str = "DBOPS_NEW_USER_PASSWORD";

async fn connect(ctx: &Ctx) -> Result<Client, ExitCode> {
    match client::connect(&ctx.profile.mongodb, ctx.timeout, ctx.insecure).await {
        Ok(mongo_client) => Ok(mongo_client),
        Err(err) => {
            eprintln!("error: {err:#}");
            Err(ExitCode::from(unix::CONNECTION_FAILED))
        }
    }
}

// --- init db ---------------------------------------------------------------

pub async fn run_init_db(ctx: &Ctx, name: &str, confirm_name: Option<&str>) -> Result<ExitCode> {
    let mongo_client = match connect(ctx).await {
        Ok(mongo_client) => mongo_client,
        Err(code) => return Ok(code),
    };

    let exists = match tokio::time::timeout(ctx.timeout, database_exists(&mongo_client, name)).await
    {
        Ok(Ok(exists)) => exists,
        Ok(Err(err)) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
        Err(_elapsed) => {
            eprintln!(
                "error: mongodb init db probe timed out after {:?}",
                ctx.timeout
            );
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    if exists {
        println!("database '{name}' already exists — nothing to do");
        return Ok(ExitCode::from(unix::SUCCESS));
    }

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Create,
            target: name.to_string(),
            detail: format!(
                "materialize database by creating placeholder collection '{INIT_MARKER_COLLECTION}'"
            ),
            estimated_records: None,
        }],
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => Ok(ExitCode::from(unix::SUCCESS)),
        GuardDecision::Declined => Ok(ExitCode::from(unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {
            let db = mongo_client.database(name);
            match tokio::time::timeout(ctx.timeout, db.create_collection(INIT_MARKER_COLLECTION))
                .await
            {
                Ok(Ok(())) => {
                    println!(
                        "database '{name}' created (materialized via placeholder collection \
                         '{INIT_MARKER_COLLECTION}')"
                    );
                    Ok(ExitCode::from(unix::SUCCESS))
                }
                Ok(Err(err)) => {
                    eprintln!("error: {err:#}");
                    Ok(ExitCode::from(unix::CONNECTION_FAILED))
                }
                Err(_elapsed) => {
                    eprintln!("error: mongodb init db timed out after {:?}", ctx.timeout);
                    Ok(ExitCode::from(unix::CONNECTION_FAILED))
                }
            }
        }
    }
}

async fn database_exists(mongo_client: &Client, name: &str) -> Result<bool> {
    let names = mongo_client
        .list_database_names()
        .await
        .context("listDatabaseNames failed")?;
    Ok(names.iter().any(|n| n == name))
}

// --- reset db ----------------------------------------------------------------

pub async fn run_reset_db(ctx: &Ctx, name: &str, confirm_name: Option<&str>) -> Result<ExitCode> {
    let mongo_client = match connect(ctx).await {
        Ok(mongo_client) => mongo_client,
        Err(code) => return Ok(code),
    };

    let probe =
        match tokio::time::timeout(ctx.timeout, probe_reset_target(&mongo_client, name)).await {
            Ok(Ok(probe)) => probe,
            Ok(Err(err)) => {
                eprintln!("error: {err:#}");
                return Ok(ExitCode::from(unix::CONNECTION_FAILED));
            }
            Err(_elapsed) => {
                eprintln!(
                    "error: mongodb reset db probe timed out after {:?}",
                    ctx.timeout
                );
                return Ok(ExitCode::from(unix::CONNECTION_FAILED));
            }
        };

    // A mistyped target must never fall through to "nothing planned, exit
    // 0" — that would make a typo silently a no-op instead of a caught
    // mistake. It's reported as a hard error, before any plan/guard step.
    let Some(probe) = probe else {
        eprintln!(
            "error: database '{name}' does not exist — refusing to drop a possibly-mistyped target"
        );
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    };

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Drop,
            target: name.to_string(),
            detail: format!(
                "drop database ({} collection{}) — mongo has no separate 'recreate' step; the \
                 database comes back implicitly on the next write or the next `mongo init db`",
                probe.collections,
                plural(probe.collections),
            ),
            estimated_records: Some(probe.documents),
        }],
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => Ok(ExitCode::from(unix::SUCCESS)),
        GuardDecision::Declined => Ok(ExitCode::from(unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {
            let db = mongo_client.database(name);
            match tokio::time::timeout(ctx.timeout, db.drop()).await {
                Ok(Ok(())) => {
                    println!(
                        "database '{name}' dropped ({} collection{}, ~{} document{} removed) — \
                         this is mongo's equivalent of reset; nothing is recreated automatically",
                        probe.collections,
                        plural(probe.collections),
                        probe.documents,
                        plural(probe.documents as i64),
                    );
                    Ok(ExitCode::from(unix::SUCCESS))
                }
                Ok(Err(err)) => {
                    eprintln!("error: {err:#}");
                    Ok(ExitCode::from(unix::CONNECTION_FAILED))
                }
                Err(_elapsed) => {
                    eprintln!("error: mongodb reset db timed out after {:?}", ctx.timeout);
                    Ok(ExitCode::from(unix::CONNECTION_FAILED))
                }
            }
        }
    }
}

struct ResetProbe {
    collections: i64,
    documents: u64,
}

/// `None` when the database doesn't exist — [`run_reset_db`] treats that as
/// a hard error rather than a silent no-op.
async fn probe_reset_target(mongo_client: &Client, name: &str) -> Result<Option<ResetProbe>> {
    if !database_exists(mongo_client, name).await? {
        return Ok(None);
    }
    let stats_doc = mongo_client
        .database(name)
        .run_command(doc! { "dbStats": 1 })
        .await
        .context("dbStats failed")?;
    let collections = bson_i64(&stats_doc, "collections").unwrap_or(0);
    let documents = bson_i64(&stats_doc, "objects").unwrap_or(0).max(0) as u64;
    Ok(Some(ResetProbe {
        collections,
        documents,
    }))
}

// --- init user -----------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn run_init_user(
    ctx: &Ctx,
    name: &str,
    role: &str,
    db_name: &str,
    password_flag: Option<&str>,
    if_not_exists: bool,
    confirm_name: Option<&str>,
) -> Result<ExitCode> {
    let password = match resolve_password(password_flag) {
        Ok(password) => password,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
        }
    };

    let mongo_client = match connect(ctx).await {
        Ok(mongo_client) => mongo_client,
        Err(code) => return Ok(code),
    };

    let exists =
        match tokio::time::timeout(ctx.timeout, user_exists(&mongo_client, db_name, name)).await {
            Ok(Ok(exists)) => exists,
            Ok(Err(err)) => {
                eprintln!("error: {err:#}");
                return Ok(ExitCode::from(unix::CONNECTION_FAILED));
            }
            Err(_elapsed) => {
                eprintln!(
                    "error: mongodb init user probe timed out after {:?}",
                    ctx.timeout
                );
                return Ok(ExitCode::from(unix::CONNECTION_FAILED));
            }
        };

    if exists {
        if if_not_exists {
            println!("user '{name}' already exists on database '{db_name}' — nothing to do");
            return Ok(ExitCode::from(unix::SUCCESS));
        }
        eprintln!(
            "error: user '{name}' already exists on database '{db_name}' — re-run with \
             --if-not-exists to treat this as success"
        );
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    }

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Create,
            target: format!("{db_name}.{name}"),
            detail: format!("createUser '{name}' on '{db_name}' with role '{role}'"),
            estimated_records: None,
        }],
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => Ok(ExitCode::from(unix::SUCCESS)),
        GuardDecision::Declined => Ok(ExitCode::from(unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {
            let cmd = doc! {
                "createUser": name,
                "pwd": password.as_str(),
                "roles": [ doc! { "role": role, "db": db_name } ],
            };
            match tokio::time::timeout(ctx.timeout, mongo_client.database(db_name).run_command(cmd))
                .await
            {
                Ok(Ok(_)) => {
                    println!("user '{name}' created on database '{db_name}' with role '{role}'");
                    Ok(ExitCode::from(unix::SUCCESS))
                }
                Ok(Err(err)) => {
                    eprintln!("error: {err:#}");
                    Ok(ExitCode::from(unix::CONNECTION_FAILED))
                }
                Err(_elapsed) => {
                    eprintln!(
                        "error: mongodb createUser timed out after {:?}",
                        ctx.timeout
                    );
                    Ok(ExitCode::from(unix::CONNECTION_FAILED))
                }
            }
        }
    }
}

/// `--password` (with a stderr warning about argv/shell-history exposure) or
/// the [`PASSWORD_ENV_VAR`] env var — never a bare default, since a missing
/// password must fail loudly rather than create a user nobody can predict
/// the credentials for.
fn resolve_password(flag: Option<&str>) -> Result<String> {
    if let Some(password) = flag {
        eprintln!(
            "warning: --password is visible in shell history and via `ps` (and to any other \
             process on this host that can read /proc); prefer setting {PASSWORD_ENV_VAR} instead"
        );
        return Ok(password.to_string());
    }
    std::env::var(PASSWORD_ENV_VAR).with_context(|| {
        format!("no password given — pass --password (not recommended) or set {PASSWORD_ENV_VAR}")
    })
}

async fn user_exists(mongo_client: &Client, db_name: &str, user_name: &str) -> Result<bool> {
    let result = mongo_client
        .database(db_name)
        .run_command(doc! { "usersInfo": user_name })
        .await
        .context("usersInfo failed")?;
    let users = result
        .get_array("users")
        .context("usersInfo response is missing 'users'")?;
    Ok(!users.is_empty())
}

// --- shared helpers ----------------------------------------------------------

/// Tolerates `Int32`, `Int64`, or `Double` wire representations — `dbStats`
/// field types vary by MongoDB version (matches [`crate::mongo::stats`]'s
/// and [`crate::mongo::connections`]'s parsing convention).
fn bson_i64(doc: &Document, key: &str) -> Option<i64> {
    doc.get_i64(key)
        .ok()
        .or_else(|| doc.get_i32(key).ok().map(i64::from))
        .or_else(|| doc.get_f64(key).ok().map(|value| value as i64))
}

fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- bson_i64 ------------------------------------------------------------

    #[test]
    fn bson_i64_tolerates_every_numeric_wire_type() {
        let doc = doc! { "a": 1i32, "b": 2i64, "c": 3.0f64 };
        assert_eq!(bson_i64(&doc, "a"), Some(1));
        assert_eq!(bson_i64(&doc, "b"), Some(2));
        assert_eq!(bson_i64(&doc, "c"), Some(3));
        assert_eq!(bson_i64(&doc, "missing"), None);
    }

    // --- plural ----------------------------------------------------------------

    #[test]
    fn plural_is_empty_only_for_exactly_one() {
        assert_eq!(plural(0), "s");
        assert_eq!(plural(1), "");
        assert_eq!(plural(2), "s");
        assert_eq!(plural(-1), "s");
    }

    // --- resolve_password ------------------------------------------------------

    #[test]
    fn resolve_password_prefers_the_flag_when_given() {
        let password = resolve_password(Some("hunter2")).unwrap();
        assert_eq!(password, "hunter2");
    }

    // Both cases below share the single `PASSWORD_ENV_VAR` name and mutate
    // real process env state, which Rust's default parallel test runner
    // would race on if they were split into two `#[test]` fns (unlike
    // `frame::secret`'s env tests, which each own a unique var name) —
    // kept as one test so the set/assert/remove sequence never interleaves
    // with another thread's env mutation.
    #[test]
    fn resolve_password_env_var_fallback_and_missing_cases() {
        // SAFETY: no other test in this crate reads or writes
        // DBOPS_NEW_USER_PASSWORD.
        unsafe { std::env::remove_var(PASSWORD_ENV_VAR) };
        let err = resolve_password(None).unwrap_err();
        assert!(err.to_string().contains("no password given"));

        unsafe { std::env::set_var(PASSWORD_ENV_VAR, "from-env") };
        let password = resolve_password(None).unwrap();
        assert_eq!(password, "from-env");
        unsafe { std::env::remove_var(PASSWORD_ENV_VAR) };
    }
}
