use std::process::ExitCode;

use clap::Parser;

mod frame;
mod mongo;
mod net;
mod os;
mod pg;
mod redis;
mod sys;

use frame::{Cli, Commands, Ctx};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let ctx = Ctx::from(&cli);

    let result = match &cli.command {
        Commands::Os(args) => os::run(args, &ctx),
        Commands::Mongo(args) => mongo::run(args, &ctx),
        Commands::Pg(args) => pg::run(args, &ctx),
        Commands::Redis(args) => redis::run(args, &ctx),
        Commands::Http(args) => net::run_http(args, &ctx),
        Commands::Tcp(args) => net::run_tcp(args, &ctx),
        Commands::Sys(args) => sys::run(args, &ctx),
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
