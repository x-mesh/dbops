use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode, HealthArgs};

pub mod client;
mod connections;
mod health;
mod init;
mod oplog;
mod replset;
mod replset_status;
mod seed;
mod stats;

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
        /// Target collection: "db.collection", or a bare collection name paired with --db
        #[arg(long)]
        collection: String,
        /// Target database, required when --collection is a bare name (no ".")
        #[arg(long)]
        db: Option<String>,
        /// NDJSON file (one JSON document per line), or a JSON array under 50MB
        #[arg(long)]
        file: PathBuf,
        /// Must match the single planned target to authorize against a protected profile
        #[arg(long = "confirm-name")]
        confirm_name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum MongoInitTarget {
    Db {
        name: String,
        /// Must match the single planned target to authorize against a protected profile
        #[arg(long = "confirm-name")]
        confirm_name: Option<String>,
    },
    User {
        name: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        db: String,
        /// Visible via `ps`/shell history. Prefer the DBOPS_NEW_USER_PASSWORD env var
        #[arg(long)]
        password: Option<String>,
        /// Treat an already-existing user as success instead of failing
        #[arg(long = "if-not-exists")]
        if_not_exists: bool,
        /// Must match the single planned target to authorize against a protected profile
        #[arg(long = "confirm-name")]
        confirm_name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum MongoResetTarget {
    Db {
        name: String,
        /// Must match the single planned target to authorize against a protected profile
        #[arg(long = "confirm-name")]
        confirm_name: Option<String>,
    },
}

pub async fn run(args: &MongoArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        MongoCommand::Health(health_args) => health::run(ctx, health_args).await,
        MongoCommand::Replset => replset::run(ctx).await,
        MongoCommand::Stats { db } => stats::run(ctx, db.as_deref()).await,
        MongoCommand::Oplog => oplog::run(ctx).await,
        MongoCommand::Connections => connections::run(ctx).await,
        MongoCommand::Init { target } => match target {
            MongoInitTarget::Db { name, confirm_name } => {
                init::run_init_db(ctx, name, confirm_name.as_deref()).await
            }
            MongoInitTarget::User {
                name,
                role,
                db,
                password,
                if_not_exists,
                confirm_name,
            } => {
                init::run_init_user(
                    ctx,
                    name,
                    role,
                    db,
                    password.as_deref(),
                    *if_not_exists,
                    confirm_name.as_deref(),
                )
                .await
            }
        },
        MongoCommand::Reset { target } => match target {
            MongoResetTarget::Db { name, confirm_name } => {
                init::run_reset_db(ctx, name, confirm_name.as_deref()).await
            }
        },
        MongoCommand::Seed {
            collection,
            db,
            file,
            confirm_name,
        } => {
            seed::run(
                ctx,
                collection,
                db.as_deref(),
                file,
                confirm_name.as_deref(),
            )
            .await
        }
    }
}
