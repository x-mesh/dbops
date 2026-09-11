//! `dbops sys check`: local host resource check (PRD R33).

use anyhow::Result;
use clap::{Args, Subcommand};
use sysinfo::{Disks, System};

use crate::frame::output::render_check;
use crate::frame::result::{CheckResult, CheckStatus, Metric};
use crate::frame::{exit, Ctx, ExitCode};

#[derive(Args, Debug)]
pub struct SysArgs {
    #[command(subcommand)]
    pub command: SysCommand,
}

#[derive(Subcommand, Debug)]
pub enum SysCommand {
    Check,
}

/// Disk-usage thresholds (discretionary). The task brief's own example
/// ("90%+ => WARNING") is folded into a two-tier scale here so there's
/// still a tier left to escalate into before the filesystem is actually
/// full.
const DISK_WARNING_PCT: f64 = 80.0;
const DISK_CRITICAL_PCT: f64 = 90.0;

/// Memory-usage thresholds, set a bit looser than disk: on Linux "used"
/// memory includes reclaimable page cache, so pinning the same bar as disk
/// would false-alarm on an otherwise healthy box.
const MEM_WARNING_PCT: f64 = 85.0;
const MEM_CRITICAL_PCT: f64 = 95.0;

pub async fn run(args: &SysArgs, ctx: &Ctx) -> Result<ExitCode> {
    match &args.command {
        SysCommand::Check => check(ctx).await,
    }
}

async fn check(ctx: &Ctx) -> Result<ExitCode> {
    let mut status = CheckStatus::Ok;
    let mut reasons: Vec<String> = Vec::new();
    let mut metrics: Vec<Metric> = Vec::new();

    collect_disks(&mut status, &mut reasons, &mut metrics);
    collect_memory(&mut status, &mut reasons, &mut metrics);
    collect_load_average(&mut metrics);
    collect_docker(&mut reasons, &mut metrics).await;

    let summary = if reasons.is_empty() {
        "disk/memory/load within thresholds".to_string()
    } else {
        reasons.join("; ")
    };

    let result = CheckResult {
        status,
        summary,
        metrics,
    };
    println!("{}", render_check("sys", "check", &result, ctx.json));
    Ok(ExitCode::from(exit::from_status(result.status)))
}

/// Per-mount disk usage. Metric names use `disk:<mount>`. Mount points
/// containing spaces (rare, but possible for e.g. external volumes) will
/// render as multiple perfdata-looking tokens in text mode; `--json`
/// output is unaffected since it isn't space-delimited.
fn collect_disks(status: &mut CheckStatus, reasons: &mut Vec<String>, metrics: &mut Vec<Metric>) {
    let disks = Disks::new_with_refreshed_list();
    for disk in disks.list() {
        let total = disk.total_space();
        if total == 0 {
            // Pseudo filesystems (some tmpfs/overlay mounts) report a
            // zero-size total; skip rather than divide by zero.
            continue;
        }
        let used = total.saturating_sub(disk.available_space());
        let pct = used as f64 / total as f64 * 100.0;
        let mount = disk.mount_point().to_string_lossy().to_string();

        let disk_status = threshold_status(pct, DISK_WARNING_PCT, DISK_CRITICAL_PCT);
        if disk_status != CheckStatus::Ok {
            *status = escalate(*status, disk_status);
            reasons.push(format!("disk {mount} at {pct:.1}%"));
        }
        metrics.push(Metric {
            name: format!("disk:{mount}"),
            value: pct,
            unit: Some("%".to_string()),
            warn: Some(DISK_WARNING_PCT.to_string()),
            crit: Some(DISK_CRITICAL_PCT.to_string()),
        });
    }
}

fn collect_memory(status: &mut CheckStatus, reasons: &mut Vec<String>, metrics: &mut Vec<Metric>) {
    let mut sys = System::new();
    sys.refresh_memory();
    let total_mem = sys.total_memory();
    if total_mem == 0 {
        return;
    }
    let mem_pct = sys.used_memory() as f64 / total_mem as f64 * 100.0;
    let mem_status = threshold_status(mem_pct, MEM_WARNING_PCT, MEM_CRITICAL_PCT);
    if mem_status != CheckStatus::Ok {
        *status = escalate(*status, mem_status);
        reasons.push(format!("memory at {mem_pct:.1}%"));
    }
    metrics.push(Metric {
        name: "mem_used_pct".to_string(),
        value: mem_pct,
        unit: Some("%".to_string()),
        warn: Some(MEM_WARNING_PCT.to_string()),
        crit: Some(MEM_CRITICAL_PCT.to_string()),
    });
}

/// Load average is reported as plain metrics, not turned into a pass/fail
/// verdict: a raw load figure only means something once divided by CPU
/// core count, and there's no single correct core-count source across
/// containers/VMs/bare metal. That normalization is left for v2.
fn collect_load_average(metrics: &mut Vec<Metric>) {
    let load = System::load_average();
    for (name, value) in [
        ("load1", load.one),
        ("load5", load.five),
        ("load15", load.fifteen),
    ] {
        metrics.push(Metric {
            name: name.to_string(),
            value,
            unit: None,
            warn: None,
            crit: None,
        });
    }
}

/// Docker container counts, best-effort and informational only (not part
/// of `status`). Skipped silently, not an error, when `docker` isn't
/// installed or the daemon isn't reachable, since a box without Docker is
/// a normal environment for this tool.
async fn collect_docker(reasons: &mut Vec<String>, metrics: &mut Vec<Metric>) {
    match docker_container_counts().await {
        Some((running, total)) => {
            metrics.push(Metric {
                name: "docker_running".to_string(),
                value: running as f64,
                unit: Some("containers".to_string()),
                warn: None,
                crit: None,
            });
            metrics.push(Metric {
                name: "docker_total".to_string(),
                value: total as f64,
                unit: Some("containers".to_string()),
                warn: None,
                crit: None,
            });
        }
        None => reasons.push("docker: unavailable".to_string()),
    }
}

/// Runs `docker ps --format json` (newline-delimited JSON, one object per
/// container) off the async runtime via `spawn_blocking`: tokio's
/// "process" feature isn't enabled in this workspace (Cargo.toml is out
/// of scope for this task), so a plain `std::process::Command` is the
/// only option, and it must not block a worker thread directly.
async fn docker_container_counts() -> Option<(usize, usize)> {
    let output = tokio::task::spawn_blocking(|| {
        std::process::Command::new("docker")
            .args(["ps", "--format", "json"])
            .output()
    })
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut total = 0usize;
    let mut running = 0usize;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        total += 1;
        if value.get("State").and_then(|s| s.as_str()) == Some("running") {
            running += 1;
        }
    }
    Some((running, total))
}

fn threshold_status(pct: f64, warn: f64, crit: f64) -> CheckStatus {
    if pct >= crit {
        CheckStatus::Critical
    } else if pct >= warn {
        CheckStatus::Warning
    } else {
        CheckStatus::Ok
    }
}

/// See `net::http::escalate` for why this exists instead of `Ord` on
/// `CheckStatus`. Duplicated rather than shared across `net`/`sys` so each
/// domain module stays self-contained per the L3 interface contract.
fn escalate(current: CheckStatus, candidate: CheckStatus) -> CheckStatus {
    fn rank(s: CheckStatus) -> u8 {
        match s {
            CheckStatus::Ok => 0,
            CheckStatus::Warning => 1,
            CheckStatus::Critical => 2,
            CheckStatus::Unknown => 3,
        }
    }
    if rank(candidate) > rank(current) {
        candidate
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_status_picks_the_right_tier() {
        assert_eq!(threshold_status(50.0, 80.0, 90.0), CheckStatus::Ok);
        assert_eq!(threshold_status(80.0, 80.0, 90.0), CheckStatus::Warning);
        assert_eq!(threshold_status(90.0, 80.0, 90.0), CheckStatus::Critical);
    }

    #[test]
    fn escalate_never_downgrades() {
        assert_eq!(
            escalate(CheckStatus::Critical, CheckStatus::Warning),
            CheckStatus::Critical
        );
        assert_eq!(
            escalate(CheckStatus::Ok, CheckStatus::Warning),
            CheckStatus::Warning
        );
    }
}
