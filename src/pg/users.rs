//! `dbops pg users list|create|grant` (R24): role listing (read-only, no
//! guard) plus role creation and grants (both destructive-capable, both go
//! through `frame::guard`).

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::frame::exit::unix;
use crate::frame::guard::{self, GuardDecision};
use crate::frame::plan::{ActionKind, PlannedAction, PlanPreview};
use crate::frame::result::StatReport;
use crate::frame::{output, Ctx, ExitCode};
use crate::pg::{client, quote_ident, validate_identifier};

const ROLES_QUERY: &str = "\
SELECT r.rolname, r.rolsuper, r.rolcreatedb, r.rolcanlogin, \
       COALESCE(array_agg(m.rolname::text) FILTER (WHERE m.rolname IS NOT NULL), '{}') \
FROM pg_roles r \
LEFT JOIN pg_auth_members am ON am.member = r.oid \
LEFT JOIN pg_roles m ON m.oid = am.roleid \
GROUP BY r.oid, r.rolname, r.rolsuper, r.rolcreatedb, r.rolcanlogin \
ORDER BY r.rolname";

// --- list ------------------------------------------------------------------

pub async fn run_list(ctx: &Ctx) -> Result<ExitCode> {
    let pg_client = match client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let rows = match tokio::time::timeout(ctx.timeout, fetch_roles(&pg_client)).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(err)) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
        Err(_) => {
            eprintln!("error: query timed out after {:?}", ctx.timeout);
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    println!("{}", output::render_stat(&build_list_report(&rows), ctx.json));
    Ok(ExitCode::from(unix::SUCCESS))
}

struct RoleRow {
    name: String,
    superuser: bool,
    createdb: bool,
    login: bool,
    member_of: Vec<String>,
}

async fn fetch_roles(pg_client: &Client) -> Result<Vec<RoleRow>> {
    let rows = pg_client
        .query(ROLES_QUERY, &[])
        .await
        .context("pg_roles query failed")?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(RoleRow {
            name: row.try_get(0).context("unexpected pg_roles row shape")?,
            superuser: row.try_get(1).context("unexpected pg_roles row shape")?,
            createdb: row.try_get(2).context("unexpected pg_roles row shape")?,
            login: row.try_get(3).context("unexpected pg_roles row shape")?,
            member_of: row.try_get(4).context("unexpected pg_roles row shape")?,
        });
    }
    Ok(out)
}

fn build_list_report(rows: &[RoleRow]) -> StatReport {
    let columns = ["role", "superuser", "createdb", "login", "member_of"]
        .into_iter()
        .map(String::from)
        .collect();

    let out_rows = rows
        .iter()
        .map(|r| {
            vec![
                r.name.clone(),
                r.superuser.to_string(),
                r.createdb.to_string(),
                r.login.to_string(),
                if r.member_of.is_empty() {
                    "-".to_string()
                } else {
                    r.member_of.join(",")
                },
            ]
        })
        .collect();

    StatReport {
        columns,
        rows: out_rows,
    }
}

// --- create ------------------------------------------------------------------

pub async fn run_create(
    ctx: &Ctx,
    name: &str,
    password_env: Option<&str>,
    login: bool,
    if_not_exists: bool,
    confirm_name: Option<&str>,
) -> Result<ExitCode> {
    if let Err(err) = validate_identifier(name) {
        eprintln!("error: {err:#}");
        return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
    }

    // Secret convention: the password is only ever accepted by reference to
    // an env var, never as a bare CLI argument that would show up in
    // `ps`/shell history.
    let password = match password_env {
        Some(var) => match std::env::var(var) {
            Ok(value) => Some(value),
            Err(_) => {
                eprintln!("error: --password-env {var} is not set in the environment");
                return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
            }
        },
        None => None,
    };

    let pg_client = match client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let already_exists = role_exists(&pg_client, name).await.unwrap_or(false);
    if already_exists && !if_not_exists {
        eprintln!("error: role {name:?} already exists (use --if-not-exists to no-op instead)");
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    }

    let login_word = if login { "LOGIN" } else { "NOLOGIN" };
    let detail = format!(
        "{login_word}, {}{}",
        if password.is_some() { "password from env" } else { "no password" },
        if already_exists {
            " -- already exists, --if-not-exists makes this a no-op"
        } else {
            ""
        },
    );

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Create,
            target: name.to_string(),
            detail,
            estimated_records: None,
        }],
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => return Ok(ExitCode::from(unix::SUCCESS)),
        GuardDecision::Declined => return Ok(ExitCode::from(unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {}
    }

    if already_exists {
        println!("role {name:?} already exists, no changes made");
        return Ok(ExitCode::from(unix::SUCCESS));
    }

    let name_q = quote_ident(name);
    let mut stmt = format!("CREATE ROLE {name_q} WITH {login_word}");
    if let Some(pw) = &password {
        // A value position, not an identifier -- standard SQL string-literal
        // escaping (doubling `'`) applies here, not quote_ident. Postgres
        // DDL has no bind-parameter slot for this (CREATE ROLE ... PASSWORD
        // takes a literal token in the grammar, not an expression), so this
        // is the correct client-side escaping rather than a workaround.
        stmt.push_str(" PASSWORD '");
        stmt.push_str(&pw.replace('\'', "''"));
        stmt.push('\'');
    }
    stmt.push(';');

    if let Err(err) = pg_client.simple_query(&stmt).await {
        eprintln!("error: CREATE ROLE failed: {err:#}");
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    }

    println!("role {name:?} created ({login_word})");
    Ok(ExitCode::from(unix::SUCCESS))
}

async fn role_exists(pg_client: &Client, name: &str) -> Result<bool> {
    let row = pg_client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)",
            &[&name],
        )
        .await
        .context("pg_roles existence check failed")?;
    row.try_get(0).context("unexpected pg_roles row shape")
}

// --- grant -------------------------------------------------------------------

pub async fn run_grant(
    ctx: &Ctx,
    name: &str,
    role: &str,
    db: Option<&str>,
    confirm_name: Option<&str>,
) -> Result<ExitCode> {
    for id in [Some(name), Some(role), db].into_iter().flatten() {
        if let Err(err) = validate_identifier(id) {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
        }
    }

    let pg_client = match client::connect(&ctx.profile.postgres, ctx.timeout, ctx.insecure).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    let already_member = role_membership(&pg_client, name, role).await.unwrap_or(false);
    let detail = match db {
        Some(db_name) => format!(
            "GRANT {role} TO {name}, plus ALL PRIVILEGES ON DATABASE {db_name} TO {name}{}",
            if already_member { " (role membership already present)" } else { "" }
        ),
        None => format!(
            "GRANT {role} TO {name}{}",
            if already_member { " (already a member)" } else { "" }
        ),
    };

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Grant,
            target: name.to_string(),
            detail,
            estimated_records: None,
        }],
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => return Ok(ExitCode::from(unix::SUCCESS)),
        GuardDecision::Declined => return Ok(ExitCode::from(unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {}
    }

    let name_q = quote_ident(name);
    let role_q = quote_ident(role);

    if let Err(err) = pg_client
        .simple_query(&format!("GRANT {role_q} TO {name_q};"))
        .await
    {
        eprintln!("error: GRANT {role} TO {name} failed: {err:#}");
        return Ok(ExitCode::from(unix::GENERAL_ERROR));
    }

    if let Some(db_name) = db {
        let db_q = quote_ident(db_name);
        if let Err(err) = pg_client
            .simple_query(&format!("GRANT ALL PRIVILEGES ON DATABASE {db_q} TO {name_q};"))
            .await
        {
            eprintln!(
                "error: role membership granted, but GRANT ON DATABASE {db_name} failed: {err:#}"
            );
            return Ok(ExitCode::from(unix::GENERAL_ERROR));
        }
    }

    // Verify against real server state rather than trusting the statements
    // above merely not having errored.
    let member_now = role_membership(&pg_client, name, role).await.unwrap_or(false);
    println!(
        "granted {role} to {name} -- membership confirmed: {}",
        if member_now { "yes" } else { "no (verification query found no membership)" }
    );

    if let Some(db_name) = db {
        let has_connect = has_database_privilege(&pg_client, name, db_name, "CONNECT")
            .await
            .unwrap_or(false);
        println!(
            "database privilege on {db_name} confirmed: {}",
            if has_connect { "yes (CONNECT)" } else { "no (verification query found no privilege)" }
        );
    }

    Ok(ExitCode::from(unix::SUCCESS))
}

async fn role_membership(pg_client: &Client, member: &str, role: &str) -> Result<bool> {
    let row = pg_client
        .query_one(
            "SELECT EXISTS( \
                SELECT 1 FROM pg_auth_members am \
                JOIN pg_roles m ON m.oid = am.member \
                JOIN pg_roles r ON r.oid = am.roleid \
                WHERE m.rolname = $1 AND r.rolname = $2 \
             )",
            &[&member, &role],
        )
        .await
        .context("pg_auth_members membership check failed")?;
    row.try_get(0).context("unexpected membership check row shape")
}

async fn has_database_privilege(
    pg_client: &Client,
    role: &str,
    db: &str,
    privilege: &str,
) -> Result<bool> {
    let row = pg_client
        .query_one(
            "SELECT has_database_privilege($1, $2, $3)",
            &[&role, &db, &privilege],
        )
        .await
        .context("has_database_privilege check failed")?;
    row.try_get(0).context("unexpected has_database_privilege row shape")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_list_report_has_expected_columns() {
        let report = build_list_report(&[]);
        assert_eq!(
            report.columns,
            vec!["role", "superuser", "createdb", "login", "member_of"]
        );
    }

    #[test]
    fn build_list_report_renders_member_of_as_comma_joined() {
        let rows = vec![RoleRow {
            name: "app".to_string(),
            superuser: false,
            createdb: false,
            login: true,
            member_of: vec!["readonly".to_string(), "readwrite".to_string()],
        }];
        let report = build_list_report(&rows);
        assert_eq!(report.rows[0][0], "app");
        assert_eq!(report.rows[0][4], "readonly,readwrite");
    }

    #[test]
    fn build_list_report_empty_membership_is_dash() {
        let rows = vec![RoleRow {
            name: "solo".to_string(),
            superuser: false,
            createdb: false,
            login: true,
            member_of: vec![],
        }];
        let report = build_list_report(&rows);
        assert_eq!(report.rows[0][4], "-");
    }
}
