use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::output::{render_check, render_stat};
use crate::frame::result::{CheckResult, CheckStatus};
use crate::frame::{exit, Ctx, ExitCode, HealthArgs};

mod client;
mod health;
mod indices;
mod nodes;
mod shards;
mod stats;

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
    Indices {
        /// Include system indices (names starting with '.'). Excluded by default.
        #[arg(long)]
        all: bool,
    },
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
        OsCommand::Indices { all } => run_indices(*all, ctx).await,
        OsCommand::Shards => run_shards(ctx).await,
        OsCommand::Stats { index } => run_stats(index.as_deref(), ctx).await,
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

async fn run_indices(all: bool, ctx: &Ctx) -> Result<ExitCode> {
    let os_client = match client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure) {
        Ok(os_client) => os_client,
        Err(err) => {
            eprintln!("error: failed to connect to opensearch: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };

    match indices::indices(&os_client, ctx.timeout, all).await {
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

/// Prints two tables (unassigned shards, then the per-node distribution
/// summary) rather than one -- see `shards::ShardsReport`'s doc comment for
/// why they aren't merged. In `--json` mode this prints two independent
/// JSON values back to back; `jq` reads a whitespace-separated stream of
/// top-level JSON values natively, so `dbops os shards --json | jq .`
/// still works without wrapping them in an array.
async fn run_shards(ctx: &Ctx) -> Result<ExitCode> {
    let os_client = match client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure) {
        Ok(os_client) => os_client,
        Err(err) => {
            eprintln!("error: failed to connect to opensearch: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };

    match shards::shards(&os_client, ctx.timeout).await {
        Ok(report) => {
            if ctx.json {
                println!("{}", render_stat(&report.unassigned, true));
                println!("{}", render_stat(&report.distribution, true));
            } else {
                println!("Unassigned shards:");
                println!("{}", render_stat(&report.unassigned, false));
                println!();
                println!("Shard distribution by node:");
                println!("{}", render_stat(&report.distribution, false));
            }
            Ok(ExitCode::from(exit::unix::SUCCESS))
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            Ok(ExitCode::from(exit::unix::CONNECTION_FAILED))
        }
    }
}

async fn run_stats(index: Option<&str>, ctx: &Ctx) -> Result<ExitCode> {
    let os_client = match client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure) {
        Ok(os_client) => os_client,
        Err(err) => {
            eprintln!("error: failed to connect to opensearch: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };

    match stats::stats(&os_client, ctx.timeout, index).await {
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
