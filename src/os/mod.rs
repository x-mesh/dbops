use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode, HealthArgs};

#[derive(Args, Debug)]
pub struct OsArgs {
    #[command(subcommand)]
    pub command: OsCommand,
}

#[derive(Subcommand, Debug)]
pub enum OsCommand {
    /// Cluster health summary
    Health(HealthArgs),
    Nodes,
    Indices,
    Shards,
    Stats {
        #[arg(long, value_name = "PATTERN")]
        index: Option<String>,
    },
    Init {
        #[command(subcommand)]
        target: OsInitTarget,
    },
    Reset {
        #[command(subcommand)]
        target: OsResetTarget,
    },
    Seed {
        #[arg(long)]
        index: String,
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum OsInitTarget {
    Index {
        name: String,
        #[arg(long)]
        mapping: PathBuf,
        #[arg(long = "if-not-exists")]
        if_not_exists: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum OsResetTarget {
    Index { name: String },
}

pub async fn run(args: &OsArgs, _ctx: &Ctx) -> Result<ExitCode> {
    anyhow::bail!("dbops os: not implemented ({:?})", args.command)
}
