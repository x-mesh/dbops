use anyhow::Result;
use clap::{Args, Subcommand};

use crate::frame::{Ctx, ExitCode};

#[derive(Args, Debug)]
pub struct SysArgs {
    #[command(subcommand)]
    pub command: SysCommand,
}

#[derive(Subcommand, Debug)]
pub enum SysCommand {
    Check,
}

pub fn run(args: &SysArgs, _ctx: &Ctx) -> Result<ExitCode> {
    anyhow::bail!("dbops sys: not implemented ({:?})", args.command)
}
