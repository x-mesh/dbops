use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode, HealthArgs};

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

pub async fn run(args: &RedisArgs, _ctx: &Ctx) -> Result<ExitCode> {
    anyhow::bail!("dbops redis: not implemented ({:?})", args.command)
}
