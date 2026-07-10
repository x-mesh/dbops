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
