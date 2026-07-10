use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode, HealthArgs};

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

pub fn run(args: &PgArgs, _ctx: &Ctx) -> Result<ExitCode> {
    anyhow::bail!("dbops pg: not implemented ({:?})", args.command)
}
