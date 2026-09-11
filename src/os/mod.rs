use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use opensearch::indices::IndicesDeleteParts;
use opensearch::{CountParts, OpenSearch};
use serde_json::Value;

use crate::frame::guard::{self, GuardDecision};
use crate::frame::output::{render_check, render_stat};
use crate::frame::plan::{ActionKind, PlanPreview, PlannedAction};
use crate::frame::result::{CheckResult, CheckStatus};
use crate::frame::{exit, Ctx, ExitCode, HealthArgs};

mod client;
mod health;
mod indices;
mod init;
mod nodes;
mod seed;
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
        /// Required to authorize this seed against a protected profile;
        /// must match the target index name (see frame::guard).
        #[arg(long = "confirm-name", value_name = "NAME")]
        confirm_name: Option<String>,
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
        /// Required to authorize this init against a protected profile;
        /// must match `name` (see frame::guard).
        #[arg(long = "confirm-name", value_name = "NAME")]
        confirm_name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum OsResetTarget {
    Index {
        name: String,
        /// Required to authorize this reset against a protected profile;
        /// must match `name` (see frame::guard).
        #[arg(long = "confirm-name", value_name = "NAME")]
        confirm_name: Option<String>,
    },
}

pub async fn run(args: &OsArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        OsCommand::Health(health_args) => run_health(health_args, ctx).await,
        OsCommand::Nodes => run_nodes(ctx).await,
        OsCommand::Indices { all } => run_indices(*all, ctx).await,
        OsCommand::Shards => run_shards(ctx).await,
        OsCommand::Stats { index } => run_stats(index.as_deref(), ctx).await,
        OsCommand::Init { target } => init::run_init(target, ctx).await,
        OsCommand::Reset { target } => run_reset(target, ctx).await,
        OsCommand::Seed {
            index,
            file,
            confirm_name,
        } => seed::run_seed(index, file, confirm_name.as_deref(), ctx).await,
    }
}

async fn run_health(health_args: &HealthArgs, ctx: &Ctx) -> Result<ExitCode> {
    // A bad --warning/--critical value is a usage error, not a connectivity
    // problem: reject it before ever touching the network, with a plain
    // stderr message and exit 3, not a nagios UNKNOWN line.
    let (warning, critical) = match health::parse_args(health_args) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("error: {err}");
            return Ok(ExitCode::from(exit::unix::ARGUMENT_ERROR));
        }
    };

    let result = match client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure) {
        Ok(os_client) => health::health(&os_client, ctx.timeout, warning, critical).await,
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
/// summary) rather than one. See `shards::ShardsReport`'s doc comment for
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

/// `dbops os reset index <name>`: drop + recreate, preserving the
/// existing mapping on a best-effort basis (see
/// [`init::fetch_mapping_for_recreate`]). A missing index is a plain
/// argument error (exit 1, no side effect) rather than a guard-gated plan:
/// there is nothing to authorize when there is nothing to drop. Everything
/// past that point (the actual drop+create) is a single two-action plan
/// sharing one `target` (the index name), per the guard contract that every
/// action in a plan must agree on the same `--confirm-name` target.
async fn run_reset(target: &OsResetTarget, ctx: &Ctx) -> Result<ExitCode> {
    let OsResetTarget::Index { name, confirm_name } = target;

    let os_client = match client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure) {
        Ok(os_client) => os_client,
        Err(err) => {
            eprintln!("error: failed to connect to opensearch: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };

    let exists = match init::index_exists(&os_client, ctx.timeout, name).await {
        Ok(exists) => exists,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };
    if !exists {
        eprintln!("error: index '{name}' does not exist");
        return Ok(ExitCode::from(exit::unix::GENERAL_ERROR));
    }

    // Best-effort: a failed doc count still lets the plan render (just
    // without an estimate) rather than blocking the reset entirely.
    let doc_count = count_docs(&os_client, ctx.timeout, name).await.ok();

    let plan = PlanPreview {
        actions: vec![
            PlannedAction {
                kind: ActionKind::Drop,
                target: name.clone(),
                detail: "drop existing index".to_string(),
                estimated_records: doc_count,
            },
            PlannedAction {
                kind: ActionKind::Create,
                target: name.clone(),
                detail: "recreate index (mapping preserved if available)".to_string(),
                estimated_records: None,
            },
        ],
    };

    match guard::authorize(ctx, &plan, confirm_name.as_deref())? {
        GuardDecision::DryRun => Ok(ExitCode::from(exit::unix::SUCCESS)),
        GuardDecision::Declined => Ok(ExitCode::from(exit::unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => match apply_reset(&os_client, ctx.timeout, name).await {
            Ok(()) => {
                println!("index '{name}' reset");
                Ok(ExitCode::from(exit::unix::SUCCESS))
            }
            Err(err) => {
                eprintln!("error: {err:#}");
                Ok(ExitCode::from(exit::unix::GENERAL_ERROR))
            }
        },
    }
}

/// Drop then recreate `name`. Captures the mapping *before* dropping (a
/// captured mapping is always applied; if the fetch fails or the index has
/// no mapping, the recreated index is empty rather than the reset aborting).
async fn apply_reset(client: &OpenSearch, timeout: Duration, name: &str) -> Result<()> {
    let mapping_body = init::fetch_mapping_for_recreate(client, timeout, name).await;

    let names = [name];
    let indices = client.indices();
    let fut = indices.delete(IndicesDeleteParts::Index(&names)).send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("indices.delete timed out")?
        .context("failed to call indices.delete")?;
    init::ensure_success(response, "indices.delete").await?;

    let body = mapping_body.unwrap_or_else(|| Value::Object(Default::default()));
    init::create_index(client, timeout, name, body).await
}

async fn count_docs(client: &OpenSearch, timeout: Duration, name: &str) -> Result<u64> {
    let names = [name];
    let fut = client.count(CountParts::Index(&names)).send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("_count timed out")?
        .context("failed to query _count")?;
    let body: Value = response
        .json()
        .await
        .context("failed to parse _count response")?;
    body.get("count")
        .and_then(Value::as_u64)
        .context("_count response missing 'count'")
}
