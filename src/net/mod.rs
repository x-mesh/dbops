use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode};

mod cert;
mod http;
mod tcp;

#[derive(Args, Debug)]
pub struct HttpArgs {
    #[command(subcommand)]
    pub command: HttpCommand,
}

#[derive(Subcommand, Debug)]
pub enum HttpCommand {
    Check {
        url: String,
        #[arg(long = "expect-status", value_name = "CODE")]
        expect_status: Option<u16>,
        /// Response-time WARNING threshold, e.g. `500ms`, `2s`.
        #[arg(long, value_name = "DUR")]
        warning: Option<String>,
        /// Response-time CRITICAL threshold, e.g. `1s`, `5s`.
        #[arg(long, value_name = "DUR")]
        critical: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct TcpArgs {
    #[command(subcommand)]
    pub command: TcpCommand,
}

#[derive(Subcommand, Debug)]
pub enum TcpCommand {
    Check {
        #[arg(value_name = "HOST:PORT")]
        address: String,
        /// Connect-time WARNING threshold, e.g. `500ms`.
        #[arg(long, value_name = "DUR")]
        warning: Option<String>,
        /// Connect-time CRITICAL threshold, e.g. `1s`.
        #[arg(long, value_name = "DUR")]
        critical: Option<String>,
    },
}

pub async fn run_http(args: &HttpArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        HttpCommand::Check { url, expect_status, warning, critical } => {
            http::check(ctx, url, *expect_status, warning.as_deref(), critical.as_deref()).await
        }
    }
}

pub async fn run_tcp(args: &TcpArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        TcpCommand::Check { address, warning, critical } => {
            tcp::check(ctx, address, warning.as_deref(), critical.as_deref()).await
        }
    }
}
