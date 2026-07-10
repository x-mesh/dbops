use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::exit::{self, from_status};
use crate::frame::output::render_check;
use crate::frame::{Ctx, ExitCode, HealthArgs};

pub mod client;
mod health;
mod init;
mod queries;
mod replication;
mod stats;
mod tables;
mod users;
mod vacuum;

#[derive(Args, Debug)]
pub struct PgArgs {
    #[command(subcommand)]
    pub command: PgCommand,
}

#[derive(Subcommand, Debug)]
pub enum PgCommand {
    /// Cluster health summary
    Health(HealthArgs),
    Stats {
        #[arg(long)]
        db: Option<String>,
    },
    Tables {
        #[arg(long, value_name = "N")]
        top: Option<u32>,
    },
    Queries {
        #[arg(long = "long-running")]
        long_running: bool,
        #[arg(long, value_name = "DUR")]
        threshold: Option<String>,
    },
    Vacuum,
    Replication,
    Init {
        #[command(subcommand)]
        target: PgInitTarget,
    },
    Users {
        #[command(subcommand)]
        action: PgUsersAction,
    },
    Reset {
        #[command(subcommand)]
        target: PgResetTarget,
    },
}

#[derive(Subcommand, Debug)]
pub enum PgInitTarget {
    /// Apply a SQL file (R23). Wrapped in a single transaction unless a
    /// transaction-incompatible statement (e.g. `CREATE INDEX
    /// CONCURRENTLY`) is detected, in which case statements run one by one
    /// and a failure is reported by statement number rather than rolled
    /// back.
    Schema {
        #[arg(long)]
        file: PathBuf,
        /// Database to apply the file against (defaults to the resolved
        /// profile's own database).
        #[arg(long)]
        db: Option<String>,
        /// Required to match the plan's target when the active profile is
        /// protected (see `frame::guard`).
        #[arg(long = "confirm-name", value_name = "NAME")]
        confirm_name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PgUsersAction {
    /// List roles with their key attributes and group memberships.
    List,
    /// Create a role (R24).
    Create {
        name: String,
        /// Env var holding the role's password; the value is never accepted
        /// as a bare CLI argument (PRD secret convention).
        #[arg(long = "password-env", value_name = "VAR")]
        password_env: Option<String>,
        /// Explicit LOGIN (this is also the default when neither flag is
        /// given).
        #[arg(long, conflicts_with = "no_login")]
        login: bool,
        /// NOLOGIN instead of the default LOGIN.
        #[arg(long = "no-login", conflicts_with = "login")]
        no_login: bool,
        /// No-op (exit 0) instead of erroring when the role already exists.
        #[arg(long = "if-not-exists")]
        if_not_exists: bool,
        #[arg(long = "confirm-name", value_name = "NAME")]
        confirm_name: Option<String>,
    },
    /// Grant role membership (and optionally database privileges) to a user.
    Grant {
        /// The role/user receiving the grant.
        name: String,
        #[arg(long)]
        role: String,
        /// Also GRANT ALL PRIVILEGES ON DATABASE <db> TO <name>.
        #[arg(long)]
        db: Option<String>,
        #[arg(long = "confirm-name", value_name = "NAME")]
        confirm_name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PgResetTarget {
    /// Drop and recreate a database (R25). Connects to the `postgres`
    /// maintenance database to perform the drop/create, since a database
    /// can't drop itself.
    Db {
        name: String,
        #[arg(long = "confirm-name", value_name = "NAME")]
        confirm_name: Option<String>,
    },
}

pub async fn run(args: &PgArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        PgCommand::Health(health_args) => run_health(ctx, health_args).await,
        PgCommand::Stats { db } => stats::run(ctx, db.as_deref()).await,
        PgCommand::Tables { top } => tables::run(ctx, *top).await,
        PgCommand::Queries {
            long_running,
            threshold,
        } => queries::run(ctx, *long_running, threshold.as_deref()).await,
        PgCommand::Vacuum => vacuum::run(ctx).await,
        PgCommand::Replication => replication::run(ctx).await,
        PgCommand::Init { target } => match target {
            PgInitTarget::Schema {
                file,
                db,
                confirm_name,
            } => init::run_schema(ctx, file, db.as_deref(), confirm_name.as_deref()).await,
        },
        PgCommand::Users { action } => match action {
            PgUsersAction::List => users::run_list(ctx).await,
            PgUsersAction::Create {
                name,
                password_env,
                login,
                no_login,
                if_not_exists,
                confirm_name,
            } => {
                let _ = login; // conflicts_with "no_login" makes `!no_login` the whole story
                users::run_create(
                    ctx,
                    name,
                    password_env.as_deref(),
                    !no_login,
                    *if_not_exists,
                    confirm_name.as_deref(),
                )
                .await
            }
            PgUsersAction::Grant {
                name,
                role,
                db,
                confirm_name,
            } => users::run_grant(ctx, name, role, db.as_deref(), confirm_name.as_deref()).await,
        },
        PgCommand::Reset { target } => match target {
            PgResetTarget::Db { name, confirm_name } => {
                init::run_reset_db(ctx, name, confirm_name.as_deref()).await
            }
        },
    }
}

async fn run_health(ctx: &Ctx, args: &HealthArgs) -> Result<ExitCode> {
    // A bad --warning/--critical value is a usage error, not a connectivity
    // problem -- reject it before ever touching the network, with a plain
    // stderr message and exit 3, not a nagios UNKNOWN line.
    let (warning, critical) = match health::parse_args(args) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(exit::unix::ARGUMENT_ERROR));
        }
    };
    let result = health::health(ctx, args, warning, critical).await;
    println!("{}", render_check("pg", "health", &result, ctx.json));
    Ok(ExitCode::from(from_status(result.status)))
}

/// Charset every postgres identifier (database/role name) this toolkit
/// interpolates into SQL text must satisfy. `tokio_postgres` has no bind
/// parameter for identifiers (`DROP DATABASE $1` isn't valid SQL), so this
/// plus [`quote_ident`] is the injection defense for every `CREATE`/`DROP`/
/// `GRANT` statement in `init`/`users`.
pub(crate) fn validate_identifier(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if first_ok && rest_ok {
        Ok(())
    } else {
        anyhow::bail!("invalid identifier {name:?}: must match ^[a-zA-Z_][a-zA-Z0-9_]*$")
    }
}

/// Quote an already-[`validate_identifier`]-checked name for interpolation
/// into SQL text. Doubling embedded `"` is defense in depth -- the charset
/// check above already rejects anything but `[a-zA-Z0-9_]`, so this never
/// actually finds a `"` to double on validated input.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_identifier_accepts_typical_names() {
        assert!(validate_identifier("app_db").is_ok());
        assert!(validate_identifier("_leading_underscore").is_ok());
        assert!(validate_identifier("CamelCase123").is_ok());
    }

    #[test]
    fn validate_identifier_rejects_leading_digit() {
        assert!(validate_identifier("1db").is_err());
    }

    #[test]
    fn validate_identifier_rejects_empty() {
        assert!(validate_identifier("").is_err());
    }

    #[test]
    fn validate_identifier_rejects_sql_injection_attempts() {
        assert!(validate_identifier("app; DROP TABLE users;--").is_err());
        assert!(validate_identifier("app\"; DROP DATABASE prod; --").is_err());
        assert!(validate_identifier("app db").is_err());
        assert!(validate_identifier("app-db").is_err());
        assert!(validate_identifier("app'db").is_err());
    }

    #[test]
    fn quote_ident_wraps_in_double_quotes() {
        assert_eq!(quote_ident("app_db"), "\"app_db\"");
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        // Not reachable through validate_identifier-checked input, but
        // quote_ident is exercised directly here as defense-in-depth
        // coverage independent of that gate.
        assert_eq!(quote_ident("weird\"name"), "\"weird\"\"name\"");
    }
}
