use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode, HealthArgs};

#[derive(Args, Debug)]
pub struct MongoArgs {
    #[command(subcommand)]
    pub command: MongoCommand,
}

#[derive(Subcommand, Debug)]
pub enum MongoCommand {
    /// Replica set health summary
    Health(HealthArgs),
    Replset,
    Stats {
        #[arg(long)]
        db: Option<String>,
    },
    Oplog,
    Connections,
    Init {
        #[command(subcommand)]
        target: MongoInitTarget,
    },
    Reset {
        #[command(subcommand)]
        target: MongoResetTarget,
    },
    Seed {
        #[arg(long)]
        collection: String,
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum MongoInitTarget {
    Db {
        name: String,
    },
    User {
        name: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        db: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum MongoResetTarget {
    Db { name: String },
}

pub async fn run(args: &MongoArgs, _ctx: &Ctx) -> Result<ExitCode> {
    anyhow::bail!("dbops mongo: not implemented ({:?})", args.command)
}
