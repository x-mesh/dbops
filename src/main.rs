use std::process::ExitCode;

use clap::Parser;

mod frame;
mod mongo;
mod net;
mod os;
mod pg;
mod redis;
mod sys;
mod update;

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

    // Handled before Ctx::build() -- a completion script needs the static
    // Cli::command() definition only, never a resolved DB profile, so this
    // must not fail (or even attempt) config/env resolution.
    if let Commands::Completion { shell } = &cli.command {
        let mut cmd = <Cli as clap::CommandFactory>::command();
        clap_complete::generate(*shell, &mut cmd, "dbops", &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }

    // Also handled before Ctx::build(), for the same reason plus one of its
    // own: a self-update touches no database, and must keep working when the
    // config file is the thing that's broken.
    if let Commands::Update(args) = &cli.command {
        return match update::run(args, &cli).await {
            Ok(code) => code,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        };
    }

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
        Commands::Completion { .. } | Commands::Update(_) => {
            unreachable!("handled before Ctx::build above")
        }
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
