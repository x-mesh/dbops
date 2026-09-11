//! Secret reference resolution: `env:VAR`, `cmd:command`, or a literal value.
//!
//! Resolved secrets are wrapped in [`Secret`] so they can be threaded through
//! `ResolvedProfile` (and eventually connection builders) without a stray
//! `{:?}`/`{}` on the containing struct ever printing the plaintext.

// Only consumed by `config::resolve` right now (see its own
// `#![allow(dead_code)]` for why that's still unreachable from `main.rs`).
// Every item here is otherwise only exercised by this module's tests.
#![allow(dead_code)]

use std::fmt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// How long a `cmd:` secret reference is allowed to run before it's treated
/// as failed. Long enough for a keychain/vault CLI round trip, short enough
/// that a hung command doesn't stall every `dbops` invocation.
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// An unresolved secret source, as written in a config value or env var:
/// `env:VAR_NAME`, `cmd:shell command`, or anything else, taken as a literal.
#[derive(Clone, PartialEq, Eq)]
pub enum SecretRef {
    Env(String),
    Cmd(String),
    Literal(String),
}

impl SecretRef {
    /// Parse a raw string into a secret reference. Never fails: anything
    /// without a recognized `env:`/`cmd:` prefix is treated as a literal.
    pub fn parse(raw: &str) -> Self {
        if let Some(var) = raw.strip_prefix("env:") {
            SecretRef::Env(var.to_string())
        } else if let Some(cmd) = raw.strip_prefix("cmd:") {
            SecretRef::Cmd(cmd.to_string())
        } else {
            SecretRef::Literal(raw.to_string())
        }
    }

    /// Resolve to the plaintext value.
    ///
    /// `env:` reads the process environment, `cmd:` runs the command through
    /// `sh -c` (5s timeout, stdout with one trailing newline trimmed),
    /// `literal` passes through unchanged.
    pub fn resolve(&self) -> Result<String> {
        match self {
            SecretRef::Env(var) => {
                std::env::var(var).with_context(|| format!("secret env var not set: {var}"))
            }
            SecretRef::Cmd(cmd) => resolve_cmd(cmd, CMD_TIMEOUT),
            SecretRef::Literal(val) => Ok(val.clone()),
        }
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            SecretRef::Env(_) => "env",
            SecretRef::Cmd(_) => "cmd",
            SecretRef::Literal(_) => "literal",
        };
        write!(f, "SecretRef({kind}: ***)")
    }
}

/// Run `cmd` via `sh -c`, killing it and returning an error if it hasn't
/// finished within `timeout`.
///
/// Implemented with a plain `std::thread` + `mpsc` channel rather than
/// `tokio::time::timeout`/`tokio::process`: this crate's tokio dependency
/// only requests the "rt-multi-thread"/"macros" features (see Cargo.toml),
/// and neither "process" nor "time" is guaranteed to be pulled in through
/// feature unification from the other dependencies. Blocking `std::process`
/// off the async runtime avoids that dependency entirely.
fn resolve_cmd(cmd: &str, timeout: Duration) -> Result<String> {
    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn secret command")?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = child
            .wait_with_output()
            .context("failed to wait for secret command");
        // Ignore send errors: the receiver already timed out and dropped.
        let _ = tx.send(result);
    });

    let output = match rx.recv_timeout(timeout) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            kill_best_effort(pid);
            bail!("secret command timed out after {}s", timeout.as_secs());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("secret command thread exited without a result")
        }
    };

    if !output.status.success() {
        bail!("secret command exited with {}", output.status);
    }
    let stdout =
        String::from_utf8(output.stdout).context("secret command output is not valid UTF-8")?;
    Ok(stdout.trim_end_matches('\n').to_string())
}

/// Best-effort cleanup of a timed-out secret command. Failure to kill it is
/// not itself an error worth surfacing. The caller already has a timeout
/// error to report.
#[cfg(unix)]
fn kill_best_effort(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

#[cfg(not(unix))]
fn kill_best_effort(_pid: u32) {}

/// A resolved secret value. Deliberately opaque in `Debug`/`Display` so it
/// can be embedded in `ResolvedProfile` (which domain modules will print for
/// `-v`/`--dry-run` diagnostics) without ever leaking the plaintext. Call
/// [`Secret::expose`] only at the point of use (building a client/connection
/// string).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_prefix() {
        match SecretRef::parse("env:DBOPS_TEST_VAR") {
            SecretRef::Env(var) => assert_eq!(var, "DBOPS_TEST_VAR"),
            other => panic!("expected Env, got {other:?}"),
        }
    }

    #[test]
    fn parse_cmd_prefix() {
        match SecretRef::parse("cmd:echo hi") {
            SecretRef::Cmd(cmd) => assert_eq!(cmd, "echo hi"),
            other => panic!("expected Cmd, got {other:?}"),
        }
    }

    #[test]
    fn parse_literal_passthrough() {
        match SecretRef::parse("hunter2") {
            SecretRef::Literal(val) => assert_eq!(val, "hunter2"),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn resolve_literal() {
        let val = SecretRef::parse("plain-value").resolve().unwrap();
        assert_eq!(val, "plain-value");
    }

    #[test]
    fn resolve_env_reads_process_env() {
        let key = "DBOPS_SECRET_TEST_ENV_RESOLVE";
        // SAFETY: `key` is a name unique to this test; no other test reads
        // or writes it, so there's no cross-test race despite the shared
        // process environment.
        unsafe { std::env::set_var(key, "topsecret") };
        let val = SecretRef::parse(&format!("env:{key}")).resolve().unwrap();
        assert_eq!(val, "topsecret");
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn resolve_env_missing_errors() {
        let err = SecretRef::parse("env:DBOPS_SECRET_TEST_ENV_MISSING_ZZZ")
            .resolve()
            .unwrap_err();
        assert!(err.to_string().contains("not set"));
    }

    #[test]
    fn resolve_cmd_runs_and_trims_output() {
        let val = SecretRef::parse("cmd:printf 'hello\\n'").resolve().unwrap();
        assert_eq!(val, "hello");
    }

    #[test]
    fn resolve_cmd_nonzero_exit_errors() {
        let err = SecretRef::parse("cmd:sh -c 'exit 3'")
            .resolve()
            .unwrap_err();
        assert!(err.to_string().contains("exited with"));
    }

    #[test]
    fn resolve_cmd_timeout() {
        // Exercise the timeout branch directly with a short timeout instead
        // of waiting out the real 5s CMD_TIMEOUT, keeping the test fast.
        let err = resolve_cmd("sleep 10", Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn default_cmd_timeout_is_five_seconds() {
        assert_eq!(CMD_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn secret_ref_debug_never_leaks_raw_value() {
        let refs = [
            SecretRef::parse("env:DBOPS_SUPER_SECRET_VAR_NAME"),
            SecretRef::parse("cmd:echo hunter2"),
            SecretRef::parse("hunter2"),
        ];
        for r in refs {
            let debug = format!("{r:?}");
            assert!(!debug.contains("hunter2"));
            assert!(!debug.contains("DBOPS_SUPER_SECRET_VAR_NAME"));
        }
    }

    #[test]
    fn secret_debug_and_display_are_masked() {
        let secret = Secret::from("hunter2".to_string());
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert_eq!(format!("{secret}"), "***");
        assert_eq!(secret.expose(), "hunter2");
    }
}
