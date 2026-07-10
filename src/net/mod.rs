use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode};

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
    },
}

pub async fn run_http(args: &HttpArgs, _ctx: &Ctx) -> Result<ExitCode> {
    anyhow::bail!("dbops http: not implemented ({:?})", args.command)
}

pub async fn run_tcp(args: &TcpArgs, _ctx: &Ctx) -> Result<ExitCode> {
    anyhow::bail!("dbops tcp: not implemented ({:?})", args.command)
}
