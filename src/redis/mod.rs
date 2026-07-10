use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode, HealthArgs};

mod connect;
mod health;
mod info;
mod keyspace;
mod replication;
mod slowlog;
mod stats;

#[derive(Args, Debug)]
pub struct RedisArgs {
    #[command(subcommand)]
    pub command: RedisCommand,
}

#[derive(Subcommand, Debug)]
pub enum RedisCommand {
    /// Instance health summary
    Health(HealthArgs),
    Stats,
    Keyspace,
    Replication,
    Slowlog {
        #[arg(long = "n", value_name = "N")]
        n: Option<u32>,
    },
}

pub async fn run(args: &RedisArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        RedisCommand::Health(health_args) => health::run(health_args, ctx).await,
        RedisCommand::Stats => stats::run(ctx).await,
        RedisCommand::Keyspace => keyspace::run(ctx).await,
        RedisCommand::Replication => replication::run(ctx).await,
        RedisCommand::Slowlog { n } => slowlog::run(*n, ctx).await,
    }
}
