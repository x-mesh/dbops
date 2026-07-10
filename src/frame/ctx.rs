use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::frame::cli::Cli;
use crate::frame::config::{self, ResolvedProfile};

pub type ExitCode = std::process::ExitCode;

/// Default timeout for every remote call (PRD R41).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolved global flags + connection profile, threaded into every domain
/// module's `run()`. Flag fields are consumed by domain modules once they
/// stop being stubs.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Ctx {
    pub profile: ResolvedProfile,
    pub config: Option<PathBuf>,
    pub json: bool,
    pub timeout: Duration,
    pub dry_run: bool,
    pub yes: bool,
    pub insecure: bool,
    pub verbose: u8,
}

impl Ctx {
    /// Resolve config (flags > env > config file) and parse global flags.
    pub fn build(cli: &Cli) -> Result<Self> {
        let env: HashMap<String, String> = std::env::vars().collect();
        let profile = config::resolve(cli, &env, cli.config.as_deref())?;
        let timeout = match &cli.timeout {
            Some(raw) => {
                parse_timeout(raw).with_context(|| format!("invalid --timeout value: {raw:?}"))?
            }
            None => DEFAULT_TIMEOUT,
        };
        Ok(Self {
            profile,
            config: cli.config.clone(),
            json: cli.json,
            timeout,
            dry_run: cli.dry_run,
            yes: cli.yes,
            insecure: cli.insecure,
            verbose: cli.verbose,
        })
    }
}

/// Parse a human-friendly duration: `500ms`, `5s`, `2m`, or bare seconds (`5`).
pub fn parse_timeout(raw: &str) -> Result<Duration> {
    let raw = raw.trim();
    let (digits, factor_ms) = if let Some(v) = raw.strip_suffix("ms") {
        (v, 1u64)
    } else if let Some(v) = raw.strip_suffix('s') {
        (v, 1_000)
    } else if let Some(v) = raw.strip_suffix('m') {
        (v, 60_000)
    } else {
        (raw, 1_000)
    };
    let n: u64 = digits.trim().parse().ok().filter(|n| *n > 0).map_or_else(
        || bail!("expected forms like 500ms, 5s, 2m, or bare seconds"),
        Ok,
    )?;
    Ok(Duration::from_millis(n * factor_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timeout_accepts_common_forms() {
        assert_eq!(parse_timeout("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_timeout("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_timeout("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_timeout("7").unwrap(), Duration::from_secs(7));
    }

    #[test]
    fn parse_timeout_rejects_garbage_and_zero() {
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("0s").is_err());
        assert!(parse_timeout("").is_err());
    }
}
