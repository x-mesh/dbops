use std::path::PathBuf;

use crate::frame::cli::Cli;

pub type ExitCode = std::process::ExitCode;

/// Resolved global flags, threaded into every domain module's `run()`.
/// Fields are consumed by domain modules once they stop being stubs.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Ctx {
    pub profile: Option<String>,
    pub config: Option<PathBuf>,
    pub json: bool,
    pub timeout: Option<String>,
    pub dry_run: bool,
    pub yes: bool,
    pub insecure: bool,
    pub verbose: u8,
}

impl From<&Cli> for Ctx {
    fn from(cli: &Cli) -> Self {
        Self {
            profile: cli.profile.clone(),
            config: cli.config.clone(),
            json: cli.json,
            timeout: cli.timeout.clone(),
            dry_run: cli.dry_run,
            yes: cli.yes,
            insecure: cli.insecure,
            verbose: cli.verbose,
        }
    }
}
