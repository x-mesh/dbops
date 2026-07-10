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

#[tokio::main]
async fn main() -> ExitCode {
    // Every TLS-capable dependency is feature-gated to the ring crypto
    // backend (see Cargo.toml); rustls needs one process-wide default
    // provider installed before any TLS connection is made.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");

    let cli = Cli::parse();
    let ctx = match Ctx::build(&cli) {
        Ok(ctx) => ctx,
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::from(frame::exit::unix::ARGUMENT_ERROR);
        }
    };

    let result = match &cli.command {
        Commands::Os(args) => os::run(args, &ctx).await,
        Commands::Mongo(args) => mongo::run(args, &ctx).await,
        Commands::Pg(args) => pg::run(args, &ctx).await,
        Commands::Redis(args) => redis::run(args, &ctx).await,
        Commands::Http(args) => net::run_http(args, &ctx).await,
        Commands::Tcp(args) => net::run_tcp(args, &ctx).await,
        Commands::Sys(args) => sys::run(args, &ctx).await,
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
