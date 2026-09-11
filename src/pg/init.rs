//! `dbops pg init schema` (R23) and `dbops pg reset db` (R25) -- the two
//! `pg` subcommands that create or destroy whole databases/schemas, grouped
//! together because both may need to connect somewhere other than the
//! resolved profile's own `dbname` (schema init via `--db`, reset always via
//! the `postgres` maintenance database).

use std::path::Path;

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::frame::config::PostgresProfile;
use crate::frame::exit::unix;
use crate::frame::guard::{self, GuardDecision};
use crate::frame::plan::{ActionKind, PlanPreview, PlannedAction};
use crate::frame::{Ctx, ExitCode};
use crate::pg::{client, quote_ident, validate_identifier};

/// Every `pg reset db` connects here first -- a database can't `DROP` (or
/// recreate) itself while a session is connected to it.
const MAINTENANCE_DB: &str = "postgres";

// --- init schema -------------------------------------------------------------

pub async fn run_schema(
    ctx: &Ctx,
    file: &Path,
    db: Option<&str>,
    confirm_name: Option<&str>,
) -> Result<ExitCode> {
    let sql = match std::fs::read_to_string(file) {
        Ok(sql) => sql,
        Err(err) => {
            eprintln!("error: failed to read SQL file {}: {err}", file.display());
            return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
        }
    };

    let statements = split_statements(&sql);
    if statements.is_empty() {
        eprintln!("error: no statements found in {}", file.display());
        return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
    }

    if let Some(db_name) = db {
        if let Err(err) = validate_identifier(db_name) {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
        }
    }

    let target = db
        .map(str::to_string)
        .unwrap_or_else(|| file_target_label(file));

    let profile = profile_for_db(ctx, db);
    let pg_client = match client::connect(&profile, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    // Real target state for the dry-run/confirmation preview, not just the
    // statement count -- an operator deciding whether to apply a schema file
    // needs to know up front whether the target already has tables in it.
    let table_count = table_count_via(&pg_client).await;
    let concurrent = statements.iter().any(|s| contains_concurrent_ddl(s));

    let mode_note = if concurrent {
        "runs statement-by-statement, no transaction wrap (CONCURRENTLY detected)"
    } else {
        "wrapped in a single transaction"
    };
    let target_note = match table_count {
        Some(n) => format!(
            "target already has {n} table{}",
            if n == 1 { "" } else { "s" }
        ),
        None => "target table count unavailable".to_string(),
    };
    let detail = format!(
        "{} statement{} from {}: {target_note}; {mode_note}",
        statements.len(),
        if statements.len() == 1 { "" } else { "s" },
        file.display(),
    );

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Create,
            target: target.clone(),
            detail,
            estimated_records: None,
        }],
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => return Ok(ExitCode::from(unix::SUCCESS)),
        GuardDecision::Declined => return Ok(ExitCode::from(unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {}
    }

    if concurrent {
        apply_statement_by_statement(&pg_client, &statements).await
    } else {
        apply_transactional(&pg_client, &statements).await
    }
}

/// Apply every statement inside one `BEGIN`/`COMMIT` via the simple query
/// protocol (which accepts a `;`-separated multi-statement batch in one
/// call). On failure, the batch aborts mid-flight -- postgres skips every
/// remaining statement including `COMMIT` once the transaction enters the
/// aborted state -- so nothing from this run is left applied; an explicit
/// `ROLLBACK` afterward just makes sure the session itself isn't left
/// holding that aborted transaction open.
async fn apply_transactional(pg_client: &Client, statements: &[String]) -> Result<ExitCode> {
    let mut batch = String::from("BEGIN;\n");
    for stmt in statements {
        batch.push_str(stmt);
        batch.push_str(";\n");
    }
    batch.push_str("COMMIT;\n");

    if let Err(err) = pg_client.simple_query(&batch).await {
        let _ = pg_client.simple_query("ROLLBACK;").await;
        eprintln!("error: schema apply failed, transaction rolled back: {err:#}");
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    }

    println!(
        "applied {} statement(s) in a single transaction",
        statements.len()
    );
    Ok(ExitCode::from(unix::SUCCESS))
}

/// Fallback for files containing transaction-incompatible DDL (e.g. `CREATE
/// INDEX CONCURRENTLY`): run each statement as its own simple query. A
/// failure is reported by 1-based statement number, and everything before it
/// stays applied -- there is no transaction to roll back.
async fn apply_statement_by_statement(
    pg_client: &Client,
    statements: &[String],
) -> Result<ExitCode> {
    for (idx, stmt) in statements.iter().enumerate() {
        if let Err(err) = pg_client.simple_query(stmt).await {
            eprintln!("error: {}번째 문장에서 실패: {err:#}", idx + 1);
            eprintln!(
                "note: {} of {} statement(s) already applied and were not rolled back \
                 (this file can't be wrapped in a transaction)",
                idx,
                statements.len()
            );
            return Ok(ExitCode::from(unix::GENERAL_ERROR));
        }
    }
    println!(
        "applied {} statement(s) individually, no transaction wrap (CONCURRENTLY detected)",
        statements.len()
    );
    Ok(ExitCode::from(unix::SUCCESS))
}

/// Best-effort table count for the connected database. `None` on any
/// failure (e.g. insufficient privilege) -- this is dry-run/plan context,
/// not something worth failing the whole command over.
async fn table_count_via(pg_client: &Client) -> Option<i64> {
    pg_client
        .query_one(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema')",
            &[],
        )
        .await
        .ok()?
        .try_get(0)
        .ok()
}

/// Plan/confirm target label when `--db` isn't given: the file's own name
/// (schema init is then implicitly against the resolved profile's own
/// database, which isn't itself a single stable string worth using as the
/// confirm-name target).
fn file_target_label(file: &Path) -> String {
    file.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file.display().to_string())
}

/// Simple check for DDL that cannot run inside a transaction block --
/// currently just `CONCURRENTLY` (`CREATE|DROP|REINDEX ... CONCURRENTLY`).
/// Not a full SQL parser: a word-boundary scan over the uppercased
/// statement text, which is enough for the form schema files in practice
/// use.
fn contains_concurrent_ddl(statement: &str) -> bool {
    statement
        .to_uppercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word == "CONCURRENTLY")
}

/// Split SQL text into individual statements on top-level `;` boundaries,
/// respecting single-quoted strings, double-quoted identifiers, `--`/`/*
/// */` comments, and dollar-quoted bodies (`$$...$$` / `$tag$...$tag$`) so a
/// `;` inside any of those never splits a statement in two. Blank/
/// comment-only chunks between two `;` are dropped.
fn split_statements(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' => {
                let start = i;
                i += 1;
                while i < chars.len() {
                    if chars[i] == c {
                        if chars.get(i + 1) == Some(&c) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                current.extend(&chars[start..i]);
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                let start = i;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                current.extend(&chars[start..i]);
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let start = i;
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
                current.extend(&chars[start..i]);
            }
            '$' => {
                if let Some(tag_end) = dollar_tag_end(&chars, i) {
                    let tag: Vec<char> = chars[i..=tag_end].to_vec();
                    let body_start = tag_end + 1;
                    let close = find_subsequence(&chars, body_start, &tag)
                        .map(|pos| pos + tag.len())
                        .unwrap_or(chars.len());
                    current.extend(&chars[i..close]);
                    i = close;
                } else {
                    current.push(c);
                    i += 1;
                }
            }
            ';' => {
                let stmt = current.trim().to_string();
                if !stmt.is_empty() {
                    statements.push(stmt);
                }
                current.clear();
                i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }

    let tail = current.trim().to_string();
    if !tail.is_empty() {
        statements.push(tail);
    }
    statements
}

/// If `chars[start]` opens a dollar-quote tag (`$$`, `$tag$`, ...), return
/// the index of the tag's closing `$`. `None` for a bare `$` that isn't
/// followed by a well-formed tag (e.g. a `$1` bind-parameter placeholder
/// inside a statement this splitter otherwise leaves untouched).
fn dollar_tag_end(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < chars.len() {
        match chars[j] {
            '$' => return Some(j),
            c if c.is_ascii_alphanumeric() || c == '_' => j += 1,
            _ => return None,
        }
    }
    None
}

/// First index >= `from` at which `needle` occurs in `chars`, or `None`.
fn find_subsequence(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || from + needle.len() > chars.len() {
        return None;
    }
    (from..=chars.len() - needle.len()).find(|&i| chars[i..i + needle.len()] == *needle)
}

// --- reset db ------------------------------------------------------------------

pub async fn run_reset_db(ctx: &Ctx, name: &str, confirm_name: Option<&str>) -> Result<ExitCode> {
    if let Err(err) = validate_identifier(name) {
        eprintln!("error: {err:#}");
        return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
    }

    let maint_profile = profile_for_db(ctx, Some(MAINTENANCE_DB));
    let pg_client = match client::connect(&maint_profile, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let existing = match database_size_and_owner(&pg_client, name).await {
        Ok(existing) => existing,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };
    let Some((size_pretty, owner)) = existing else {
        // Checked before authorize()/any plan is even built: a nonexistent
        // db can't be previewed or applied, so this short-circuits with no
        // change made regardless of --dry-run.
        eprintln!("error: database {name:?} does not exist");
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    };

    // Best-effort: table count needs its own connection straight into the
    // target db (a single postgres session can't introspect another
    // database's information_schema).
    let table_count =
        match client::connect(&profile_for_db(ctx, Some(name)), ctx.timeout, ctx.insecure).await {
            Ok(target_client) => table_count_via(&target_client).await,
            Err(_) => None,
        };
    let table_note = match table_count {
        Some(n) => format!("{n} table{}", if n == 1 { "" } else { "s" }),
        None => "table count unavailable".to_string(),
    };

    let plan = PlanPreview {
        actions: vec![
            PlannedAction {
                kind: ActionKind::Drop,
                target: name.to_string(),
                detail: format!("{size_pretty}, {table_note}, owner {owner}"),
                estimated_records: None,
            },
            PlannedAction {
                kind: ActionKind::Create,
                target: name.to_string(),
                detail: format!("recreate empty, owner {owner}"),
                estimated_records: None,
            },
        ],
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => return Ok(ExitCode::from(unix::SUCCESS)),
        GuardDecision::Declined => return Ok(ExitCode::from(unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {}
    }

    let name_q = quote_ident(name);

    // PG13+ `WITH (FORCE)` disconnects any other session on the target db
    // instead of failing the drop -- consistent with this toolkit's PG13
    // baseline (see pg::health's MIN_SUPPORTED_MAJOR_VERSION).
    if let Err(err) = pg_client
        .simple_query(&format!("DROP DATABASE {name_q} WITH (FORCE);"))
        .await
    {
        eprintln!("error: DROP DATABASE failed, no changes applied: {err:#}");
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    }

    if let Err(err) = recreate_with_owner(&pg_client, &name_q, &owner).await {
        eprintln!("error: database dropped but CREATE DATABASE failed: {err:#}");
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    }

    println!("database {name:?} reset (owner {owner})");
    Ok(ExitCode::from(unix::SUCCESS))
}

/// Recreate a just-dropped database, trying to preserve its original owner
/// first and falling back to the connecting role as owner (postgres's own
/// default) if that specific grant fails -- e.g. the connecting role lacks
/// `CREATEROLE`/superuser to assign ownership to another role. The database
/// not existing at all is worse than existing with the "wrong" owner, so
/// this degrades rather than leaving the db dropped.
async fn recreate_with_owner(pg_client: &Client, name_q: &str, owner: &str) -> Result<()> {
    let owner_q = quote_ident(owner);
    if let Err(err) = pg_client
        .simple_query(&format!("CREATE DATABASE {name_q} OWNER {owner_q};"))
        .await
    {
        eprintln!(
            "warning: CREATE DATABASE with OWNER {owner} failed ({err:#}); \
             retrying without an explicit owner"
        );
        pg_client
            .simple_query(&format!("CREATE DATABASE {name_q};"))
            .await
            .map(|_| ())
            .context("CREATE DATABASE (no explicit owner) also failed")?;
    }
    Ok(())
}

/// `(pg_size_pretty(...), owner rolname)` for `name`, or `None` if no such
/// database exists.
async fn database_size_and_owner(
    pg_client: &Client,
    name: &str,
) -> Result<Option<(String, String)>> {
    let row = pg_client
        .query_opt(
            "SELECT pg_size_pretty(pg_database_size(d.oid)), pg_get_userbyid(d.datdba) \
             FROM pg_database d WHERE d.datname = $1",
            &[&name],
        )
        .await
        .context("pg_database lookup failed")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let size_pretty: String = row.try_get(0).context("unexpected pg_database row shape")?;
    let owner: String = row.try_get(1).context("unexpected pg_database row shape")?;
    Ok(Some((size_pretty, owner)))
}

/// `PostgresProfile` is owned by `frame::config` and off-limits to edit from
/// this module, so every db-override call site clones the resolved
/// profile's postgres block and swaps the one field it needs -- rather than
/// connecting with the profile's own (possibly wrong-for-this-command)
/// `dbname`.
fn profile_for_db(ctx: &Ctx, db_override: Option<&str>) -> PostgresProfile {
    let mut profile = ctx.profile.postgres.clone();
    if let Some(db) = db_override {
        profile.dbname = Some(db.to_string());
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_simple_statements() {
        let stmts = split_statements("CREATE TABLE a (id int); CREATE TABLE b (id int);");
        assert_eq!(
            stmts,
            vec!["CREATE TABLE a (id int)", "CREATE TABLE b (id int)"]
        );
    }

    #[test]
    fn ignores_semicolon_inside_single_quoted_string() {
        let stmts = split_statements("INSERT INTO t (s) VALUES ('a;b');");
        assert_eq!(stmts, vec!["INSERT INTO t (s) VALUES ('a;b')"]);
    }

    #[test]
    fn handles_doubled_quote_escape_inside_string() {
        let stmts = split_statements("INSERT INTO t (s) VALUES ('it''s; here');");
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("it''s; here"));
    }

    #[test]
    fn ignores_semicolon_inside_dollar_quoted_body() {
        let sql =
            "CREATE FUNCTION f() RETURNS void AS $$ BEGIN SELECT 1; END; $$ LANGUAGE plpgsql;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("SELECT 1; END;"));
    }

    #[test]
    fn ignores_semicolon_inside_tagged_dollar_quoted_body() {
        let sql = "CREATE FUNCTION f() RETURNS void AS $body$ SELECT 1; $body$ LANGUAGE sql;";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn ignores_semicolon_in_line_comment() {
        let sql = "-- comment; still comment\nCREATE TABLE a (id int);";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("CREATE TABLE a (id int)"));
    }

    #[test]
    fn empty_and_whitespace_only_input_yields_no_statements() {
        assert!(split_statements("").is_empty());
        assert!(split_statements("   \n\t  ").is_empty());
        assert!(split_statements(";;;").is_empty());
    }

    #[test]
    fn detects_concurrently_case_insensitively() {
        assert!(contains_concurrent_ddl(
            "create index concurrently idx on t(a)"
        ));
        assert!(contains_concurrent_ddl(
            "CREATE INDEX CONCURRENTLY idx ON t(a)"
        ));
        assert!(!contains_concurrent_ddl("CREATE INDEX idx ON t(a)"));
        // must not false-positive on a substring match
        assert!(!contains_concurrent_ddl("-- run concurrently_ish later"));
    }

    #[test]
    fn file_target_label_uses_basename() {
        assert_eq!(
            file_target_label(Path::new("/tmp/dir/schema.sql")),
            "schema.sql"
        );
    }
}
