//! Which published release artifact this host should install.

use anyhow::{bail, Result};

/// The three triples `.github/workflows/release.yml` actually publishes.
pub const DARWIN_ARM64: &str = "aarch64-apple-darwin";
pub const LINUX_X86_64: &str = "x86_64-unknown-linux-musl";
pub const LINUX_ARM64: &str = "aarch64-unknown-linux-musl";

/// Name of the release asset holding `<sha256>  <asset-name>` lines.
pub const CHECKSUM_ASSET: &str = "SHA256SUMS";

/// Map an OS + CPU architecture onto the artifact triple to download.
///
/// Keyed on OS + arch rather than on this binary's own compile-time target
/// triple, on purpose: a locally built `x86_64-unknown-linux-gnu` dbops has
/// no matching artifact, and should update itself from the statically linked
/// musl one, which runs on a glibc host just as well.
pub fn asset_target(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok(DARWIN_ARM64),
        ("linux", "x86_64") => Ok(LINUX_X86_64),
        ("linux", "aarch64") => Ok(LINUX_ARM64),
        // The release matrix publishes no Intel macOS artifact. Say so,
        // rather than hand this host an arm64 binary it cannot exec.
        ("macos", "x86_64") => bail!(
            "no published artifact for Intel macOS (x86_64-apple-darwin); \
             build from source with `cargo build --release`"
        ),
        _ => bail!("unsupported platform: {os}/{arch} (dbops publishes macOS arm64 and Linux x86_64/arm64)"),
    }
}

/// [`asset_target`] for the host this binary was compiled for.
pub fn host_target() -> Result<&'static str> {
    asset_target(std::env::consts::OS, std::env::consts::ARCH)
}

/// Must stay identical to the artifact naming in `scripts/release-build.sh`'s
/// `package()`. That script writes the files this function looks up.
pub fn asset_name(version: &str, target: &str) -> String {
    format!("dbops-{version}-{target}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_published_triple_is_reachable() {
        assert_eq!(asset_target("macos", "aarch64").unwrap(), DARWIN_ARM64);
        assert_eq!(asset_target("linux", "x86_64").unwrap(), LINUX_X86_64);
        assert_eq!(asset_target("linux", "aarch64").unwrap(), LINUX_ARM64);
    }

    /// A glibc build has no artifact of its own and must fall back to the
    /// static musl one, not error out.
    #[test]
    fn linux_arch_alone_decides_the_artifact() {
        assert!(asset_target("linux", "x86_64").unwrap().ends_with("-musl"));
        assert!(asset_target("linux", "aarch64").unwrap().ends_with("-musl"));
    }

    #[test]
    fn intel_macos_names_the_missing_artifact_instead_of_guessing() {
        let err = asset_target("macos", "x86_64").unwrap_err().to_string();
        assert!(err.contains("x86_64-apple-darwin"), "got: {err}");
        assert!(err.contains("build from source"), "got: {err}");
    }

    #[test]
    fn unknown_platforms_are_rejected() {
        assert!(asset_target("windows", "x86_64").is_err());
        assert!(asset_target("linux", "riscv64").is_err());
    }

    #[test]
    fn asset_name_matches_release_build_sh() {
        assert_eq!(
            asset_name("0.1.0", LINUX_X86_64),
            "dbops-0.1.0-x86_64-unknown-linux-musl"
        );
    }

    /// The host this test runs on must be one dbops actually ships for,
    /// which is exactly the CI + release matrix.
    #[test]
    fn host_target_resolves_on_supported_hosts() {
        assert!(host_target().is_ok());
    }
}
