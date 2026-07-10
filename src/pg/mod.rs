use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::exit::from_status;
use crate::frame::output::render_check;
use crate::frame::{Ctx, ExitCode, HealthArgs};

pub mod client;
mod health;

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
    Schema {
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum PgUsersAction {
    List,
    Create {
        name: String,
    },
    Grant {
        name: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        db: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PgResetTarget {
    Db { name: String },
}

pub async fn run(args: &PgArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        PgCommand::Health(health_args) => run_health(ctx, health_args).await,
        other => anyhow::bail!("dbops pg: not implemented ({other:?})"),
    }
}

async fn run_health(ctx: &Ctx, args: &HealthArgs) -> Result<ExitCode> {
    let result = health::health(ctx, args).await;
    println!("{}", render_check("pg", "health", &result, ctx.json));
    Ok(ExitCode::from(from_status(result.status)))
}
