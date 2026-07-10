use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::output::{render_check, render_stat};
use crate::frame::result::{CheckResult, CheckStatus};
use crate::frame::{exit, Ctx, ExitCode, HealthArgs};

mod client;
mod health;
mod nodes;

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

pub async fn run(args: &OsArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        OsCommand::Health(health_args) => run_health(health_args, ctx).await,
        OsCommand::Nodes => run_nodes(ctx).await,
        other => anyhow::bail!("dbops os: not implemented ({other:?})"),
    }
}

async fn run_health(health_args: &HealthArgs, ctx: &Ctx) -> Result<ExitCode> {
    let result = match client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure) {
        Ok(os_client) => health::health(&os_client, ctx.timeout, health_args).await,
        Err(err) => CheckResult {
            status: CheckStatus::Unknown,
            summary: format!("failed to connect to opensearch: {err:#}"),
            metrics: vec![],
        },
    };
    println!("{}", render_check("os", "health", &result, ctx.json));
    Ok(ExitCode::from(exit::from_status(result.status)))
}

async fn run_nodes(ctx: &Ctx) -> Result<ExitCode> {
    let os_client = match client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure) {
        Ok(os_client) => os_client,
        Err(err) => {
            eprintln!("error: failed to connect to opensearch: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };

    match nodes::nodes(&os_client, ctx.timeout).await {
        Ok(report) => {
            println!("{}", render_stat(&report, ctx.json));
            Ok(ExitCode::from(exit::unix::SUCCESS))
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            Ok(ExitCode::from(exit::unix::CONNECTION_FAILED))
        }
    }
}
