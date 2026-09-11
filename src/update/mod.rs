//! `dbops update`: replace the running binary with a published release.
//!
//! The same job `install.sh` does for the first copy, done from inside the
//! binary: resolve the newest release for this platform, download it, check
//! its SHA-256 against the release's own `SHA256SUMS`, and swap it into
//! place atomically.
//!
//! This is the one subcommand that touches no database, so it takes `&Cli`
//! rather than a [`Ctx`](crate::frame::Ctx) and runs before profile
//! resolution. A broken `~/.dbops.toml` is exactly the kind of moment when
//! being able to update anyway matters.

mod apply;
mod github;
mod target;
mod version;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::frame::cli::Cli;
use crate::frame::ctx::parse_timeout;
use crate::frame::{exit, ExitCode};

/// Metadata-call timeout when `--timeout` isn't given. Deliberately not
/// [`frame::ctx::DEFAULT_TIMEOUT`](crate::frame::ctx::DEFAULT_TIMEOUT) (5s):
/// that number is tuned for probing a database on the same network, not for
/// a round trip to api.github.com from wherever this box happens to be.
const DEFAULT_API_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Install this exact release tag (e.g. v0.2.0), even if it is older
    /// than the running binary
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,

    /// Re-download and reinstall even when already at the target version
    #[arg(long)]
    pub force: bool,
}

/// What the run did: the field to branch on when parsing `--json` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Action {
    /// The binary at `install_path` was replaced.
    Installed,
    /// Nothing to do, and nothing was written.
    UpToDate,
    /// `--dry-run`: this is what a real run would have done.
    Planned,
}

#[derive(Debug, Serialize)]
struct UpdateReport {
    action: Action,
    current_version: String,
    /// The version that was (or would be) installed: the resolved `--tag`
    /// when one was given, which is not necessarily the newest release.
    target_version: String,
    /// Strictly "the resolved release is newer than the running binary".
    /// A `--tag` downgrade reports `false` here and still installs; read
    /// `action` to find out what happened.
    update_available: bool,
    target: String,
    asset: String,
    install_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

pub async fn run(args: &UpdateArgs, cli: &Cli) -> Result<ExitCode> {
    let api_timeout = match &cli.timeout {
        Some(raw) => {
            parse_timeout(raw).with_context(|| format!("invalid --timeout value: {raw:?}"))?
        }
        None => DEFAULT_API_TIMEOUT,
    };

    let target = target::host_target()?;
    let current = version::parse(env!("CARGO_PKG_VERSION"))?;
    let install_path = apply::running_binary()?;

    let client = github::Client::new(api_timeout)?;
    step(cli, || {
        let which = args.tag.as_deref().unwrap_or("latest");
        format!("resolving {which} release of {}", client.repo())
    });

    let release = client.release(args.tag.as_deref()).await?;
    let resolved = version::parse(&release.tag_name)
        .with_context(|| format!("release tag {:?} is not a version", release.tag_name))?;

    // Built from the normalized version rather than from the raw tag: the
    // artifacts are named after the Cargo version (`dbops-0.2.0-…`), so a
    // `v0.2` tag still has to look up `dbops-0.2.0-…`.
    let asset_name = target::asset_name(&resolved.to_string(), target);

    let mut report = UpdateReport {
        action: Action::UpToDate,
        current_version: current.to_string(),
        target_version: resolved.to_string(),
        update_available: resolved > current,
        target: target.to_string(),
        asset: asset_name.clone(),
        install_path: install_path.display().to_string(),
        sha256: None,
    };

    if !should_install(&current, &resolved, args.tag.is_some(), args.force) {
        emit(&report, cli.json);
        return Ok(ExitCode::from(exit::unix::SUCCESS));
    }

    if cli.dry_run {
        report.action = Action::Planned;
        emit(&report, cli.json);
        return Ok(ExitCode::from(exit::unix::SUCCESS));
    }

    let asset = release.asset(&asset_name).with_context(|| {
        format!(
            "release {} publishes no asset named {asset_name} (it has: {})",
            release.tag_name,
            release.asset_names().join(", ")
        )
    })?;
    let sums_asset = release.asset(target::CHECKSUM_ASSET).with_context(|| {
        format!(
            "release {} publishes no {} asset, so {asset_name} cannot be verified",
            release.tag_name,
            target::CHECKSUM_ASSET
        )
    })?;

    // Checksums first: it is the smaller download, and a release missing
    // them should fail before pulling megabytes.
    let sums = String::from_utf8(client.download(sums_asset).await?)
        .with_context(|| format!("{} is not valid UTF-8", target::CHECKSUM_ASSET))?;

    step(cli, || format!("downloading {asset_name}"));
    let bytes = client.download(asset).await?;

    let digest = apply::verify_checksum(&bytes, &sums, &asset_name)?;
    step(cli, || format!("checksum ok ({})", &digest[..12]));

    apply::replace_binary(&install_path, &bytes)?;

    report.action = Action::Installed;
    report.sha256 = Some(digest);
    emit(&report, cli.json);
    Ok(ExitCode::from(exit::unix::SUCCESS))
}

/// Whether `resolved` should be downloaded and written over the running
/// binary.
///
/// An explicit `--tag` (`pinned`) is a pin, not a suggestion: it installs
/// exactly what it names, a deliberate downgrade included, and only an
/// *identical* version is a no-op. Without one, the only release worth
/// installing is a strictly newer one, which also means a locally built
/// binary ahead of the newest release is left alone. `--force` overrides
/// both, so `dbops update --force` always re-fetches.
fn should_install(
    current: &version::Version,
    resolved: &version::Version,
    pinned: bool,
    force: bool,
) -> bool {
    if force {
        return true;
    }
    if pinned {
        resolved != current
    } else {
        resolved > current
    }
}

/// Progress line, on stderr so it never mixes into piped `--json` output.
/// Silent under `--json`, which is meant to be consumed by a machine.
fn step<F: FnOnce() -> String>(cli: &Cli, message: F) {
    if !cli.json {
        eprintln!("==> {}", message());
    }
}

fn emit(report: &UpdateReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report)
                .expect("UpdateReport contains only JSON-safe types")
        );
    } else {
        println!("{}", render_text(report));
    }
}

fn render_text(report: &UpdateReport) -> String {
    let UpdateReport {
        current_version: current,
        target_version: wanted,
        install_path: path,
        target,
        ..
    } = report;

    match report.action {
        Action::UpToDate => {
            format!("dbops {current} is up to date ({target}; newest release {wanted})")
        }
        Action::Planned => {
            format!("dbops {current} -> {wanted} ({target}); --dry-run: {path} left untouched")
        }
        Action::Installed => {
            let digest = report.sha256.as_deref().unwrap_or("");
            format!(
                "dbops {current} -> {wanted} ({target}) installed to {path} (sha256 {})",
                &digest[..digest.len().min(12)]
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(raw: &str) -> version::Version {
        version::parse(raw).unwrap()
    }

    /// `dbops update` with no flags: newer wins, nothing else moves.
    #[test]
    fn plain_update_installs_only_a_strictly_newer_release() {
        assert!(should_install(&v("0.1.0"), &v("0.2.0"), false, false));
        assert!(!should_install(&v("0.1.0"), &v("0.1.0"), false, false));
        // A locally built binary ahead of the newest release stays put.
        assert!(!should_install(&v("0.3.0"), &v("0.2.0"), false, false));
    }

    /// `--tag` names the release to run; an older one is a deliberate
    /// downgrade, not a mistake to be silently ignored.
    #[test]
    fn a_pinned_tag_installs_what_it_names_including_a_downgrade() {
        assert!(should_install(&v("0.3.0"), &v("0.2.0"), true, false));
        assert!(should_install(&v("0.1.0"), &v("0.2.0"), true, false));
        assert!(!should_install(&v("0.2.0"), &v("0.2.0"), true, false));
    }

    /// `--force` is what re-fetches a version you already have: the only
    /// way to repair a corrupted install without changing versions.
    #[test]
    fn force_reinstalls_the_version_already_running() {
        assert!(should_install(&v("0.2.0"), &v("0.2.0"), false, true));
        assert!(should_install(&v("0.2.0"), &v("0.2.0"), true, true));
        assert!(should_install(&v("0.3.0"), &v("0.2.0"), false, true));
    }

    /// A pre-release is not an upgrade over the release it precedes.
    #[test]
    fn a_prerelease_never_replaces_its_own_final_release() {
        assert!(!should_install(&v("0.2.0"), &v("0.2.0-rc.1"), false, false));
        assert!(should_install(&v("0.2.0-rc.1"), &v("0.2.0"), false, false));
    }

    fn report(action: Action) -> UpdateReport {
        UpdateReport {
            action,
            current_version: "0.1.0".to_string(),
            target_version: "0.2.0".to_string(),
            update_available: true,
            target: "aarch64-apple-darwin".to_string(),
            asset: "dbops-0.2.0-aarch64-apple-darwin".to_string(),
            install_path: "/usr/local/bin/dbops".to_string(),
            sha256: Some("ba7816bf8f01cfea414140de5dae2223".to_string()),
        }
    }

    #[test]
    fn json_report_exposes_the_fields_a_script_branches_on() {
        let json = serde_json::to_string(&report(Action::Installed)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["action"], "installed");
        assert_eq!(value["current_version"], "0.1.0");
        assert_eq!(value["target_version"], "0.2.0");
        assert_eq!(value["update_available"], true);
        assert_eq!(value["install_path"], "/usr/local/bin/dbops");
    }

    #[test]
    fn action_serializes_to_the_documented_vocabulary() {
        for (action, text) in [
            (Action::Installed, "\"installed\""),
            (Action::UpToDate, "\"up-to-date\""),
            (Action::Planned, "\"planned\""),
        ] {
            assert_eq!(serde_json::to_string(&action).unwrap(), text);
        }
    }

    /// No digest is computed on a path that never downloaded anything, so
    /// the field must vanish rather than serialize as `null`.
    #[test]
    fn absent_sha256_is_omitted_from_json() {
        let mut report = report(Action::UpToDate);
        report.sha256 = None;
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("sha256"), "got: {json}");
    }

    #[test]
    fn dry_run_text_says_nothing_was_written() {
        let text = render_text(&report(Action::Planned));
        assert!(text.contains("0.1.0 -> 0.2.0"), "got: {text}");
        assert!(
            text.contains("/usr/local/bin/dbops left untouched"),
            "got: {text}"
        );
    }

    #[test]
    fn installed_text_names_the_path_and_a_short_digest() {
        let text = render_text(&report(Action::Installed));
        assert!(
            text.contains("installed to /usr/local/bin/dbops"),
            "got: {text}"
        );
        assert!(text.contains("sha256 ba7816bf8f01"), "got: {text}");
    }

    /// A truncated digest must not panic the renderer on a short input.
    #[test]
    fn installed_text_survives_a_short_or_missing_digest() {
        let mut report = report(Action::Installed);
        report.sha256 = Some("abc".to_string());
        assert!(render_text(&report).contains("sha256 abc"));

        report.sha256 = None;
        assert!(render_text(&report).contains("sha256 "));
    }
}
