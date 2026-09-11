//! Verify a downloaded artifact, then swap it in for the running binary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Mode of the installed binary: what `scripts/release-build.sh`'s `chmod +x`
/// and `install.sh` both leave behind, independent of the caller's umask.
const BINARY_MODE: u32 = 0o755;

/// Lowercase hex SHA-256 of `bytes`.
///
/// `ring` rather than a new hashing crate: rustls already pins ring as its
/// crypto backend (see Cargo.toml), so this adds no code to the binary.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// Look up `asset`'s expected digest in the body of a `SHA256SUMS` file.
///
/// Two producers write that file with different path spellings: the local
/// `scripts/release-build.sh` emits bare names (`dbops-0.1.0-…`), while the
/// release workflow's `find | xargs shasum` emits `./`-prefixed ones. Match
/// on the final path component so both parse; strip a leading `*`, which is
/// how `sha256sum` marks a binary-mode entry.
pub fn expected_digest(sums: &str, asset: &str) -> Result<String> {
    for line in sums.lines() {
        let mut fields = line.split_whitespace();
        let (Some(digest), Some(path)) = (fields.next(), fields.next()) else {
            continue;
        };
        let path = path.trim_start_matches('*');
        let name = path.rsplit('/').next().unwrap_or(path);
        if name == asset {
            return Ok(digest.to_ascii_lowercase());
        }
    }
    bail!("no SHA256SUMS entry for {asset}")
}

/// Confirm `bytes` is the artifact `SHA256SUMS` says it is, returning its
/// digest.
///
/// This catches a truncated or corrupted download; it is not a signature.
/// `SHA256SUMS` ships inside the same release as the binary, so anyone who
/// could tamper with one could tamper with the other. HTTPS to
/// api.github.com is what establishes the release's authenticity.
pub fn verify_checksum(bytes: &[u8], sums: &str, asset: &str) -> Result<String> {
    let expected = expected_digest(sums, asset)?;
    let actual = sha256_hex(bytes);
    if actual != expected {
        bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }
    Ok(actual)
}

/// Absolute, symlink-resolved path of the running binary: the file
/// [`replace_binary`] will overwrite.
///
/// Resolving the symlink matters: a `~/.local/bin/dbops` that points into a
/// versioned directory would otherwise have the *link* replaced, leaving the
/// real binary stale and the link no longer a link.
pub fn running_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locate the running dbops binary")?;
    exe.canonicalize()
        .with_context(|| format!("resolve {}", exe.display()))
}

/// Replace `dest` with `bytes`, atomically.
///
/// The new binary is staged in `dest`'s own directory so the final `rename`
/// is a same-filesystem, atomic operation, then renamed over `dest`. On unix
/// that is safe even though `dest` is the executable currently running:
/// `rename` only swaps the directory entry, and this process keeps its
/// already-open inode until it exits.
///
/// The old binary is never moved aside first. A rename-away-then-rename-in
/// dance has a window where a crash leaves nothing at `dest` at all; a single
/// rename has no such window.
pub fn replace_binary(dest: &Path, bytes: &[u8]) -> Result<()> {
    let dir = dest
        .parent()
        .with_context(|| format!("{} has no parent directory", dest.display()))?;
    let file_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("dbops");
    let staged = dir.join(format!(".{file_name}.update.{}", std::process::id()));

    let result = write_executable(&staged, bytes).and_then(|()| {
        fs::rename(&staged, dest).with_context(|| format!("install {}", dest.display()))
    });

    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result.with_context(|| permission_hint(dir))
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(BINARY_MODE))
        .with_context(|| format!("chmod {BINARY_MODE:o} {}", path.display()))
}

/// The failure here is nearly always "dbops lives in a root-owned bin dir";
/// both ways out are worth naming.
fn permission_hint(dir: &Path) -> String {
    format!(
        "cannot write into {}. Re-run as root (`sudo dbops update`), or reinstall dbops \
         somewhere you own (`DBOPS_INSTALL_DIR=$HOME/.local/bin`)",
        dir.display()
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// Digest of the empty input, from the SHA-256 spec's own test vectors.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    /// `printf 'abc' | sha256sum`
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "dbops-update-test-{}-{n}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_matches_the_published_test_vectors() {
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
        assert_eq!(sha256_hex(b"abc"), ABC_SHA256);
    }

    /// The release workflow and `scripts/release-build.sh` spell the same
    /// path two different ways; both have to resolve.
    #[test]
    fn expected_digest_reads_both_producers_spellings() {
        let sums = format!(
            "{ABC_SHA256}  ./dbops-0.1.0-aarch64-apple-darwin\n\
             {EMPTY_SHA256}  dbops-0.1.0-x86_64-unknown-linux-musl\n"
        );
        assert_eq!(
            expected_digest(&sums, "dbops-0.1.0-aarch64-apple-darwin").unwrap(),
            ABC_SHA256
        );
        assert_eq!(
            expected_digest(&sums, "dbops-0.1.0-x86_64-unknown-linux-musl").unwrap(),
            EMPTY_SHA256
        );
    }

    #[test]
    fn expected_digest_strips_the_binary_mode_marker() {
        let sums = format!("{ABC_SHA256} *dbops-0.1.0-aarch64-apple-darwin\n");
        assert_eq!(
            expected_digest(&sums, "dbops-0.1.0-aarch64-apple-darwin").unwrap(),
            ABC_SHA256
        );
    }

    /// One triple's entry must never satisfy another's lookup.
    #[test]
    fn expected_digest_requires_a_whole_name_match() {
        let sums = format!("{ABC_SHA256}  dbops-0.1.0-aarch64-unknown-linux-musl\n");
        assert!(expected_digest(&sums, "dbops-0.1.0-x86_64-unknown-linux-musl").is_err());
        assert!(expected_digest(&sums, "linux-musl").is_err());
        assert!(expected_digest("", "dbops-0.1.0-aarch64-apple-darwin").is_err());
    }

    #[test]
    fn verify_checksum_accepts_the_matching_artifact_and_rejects_a_tampered_one() {
        let sums = format!("{ABC_SHA256}  dbops-0.1.0-linux\n");
        assert_eq!(
            verify_checksum(b"abc", &sums, "dbops-0.1.0-linux").unwrap(),
            ABC_SHA256
        );

        let err = verify_checksum(b"abd", &sums, "dbops-0.1.0-linux")
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "got: {err}");
    }

    #[test]
    fn verify_checksum_is_case_insensitive_about_the_expected_digest() {
        let sums = format!("{}  dbops-0.1.0-linux\n", ABC_SHA256.to_uppercase());
        assert!(verify_checksum(b"abc", &sums, "dbops-0.1.0-linux").is_ok());
    }

    #[test]
    fn replace_binary_overwrites_in_place_with_mode_0755() {
        let dir = unique_temp_dir();
        let dest = dir.join("dbops");
        fs::write(&dest, b"old").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();

        replace_binary(&dest, b"new").unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"new");
        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, BINARY_MODE);

        fs::remove_dir_all(&dir).ok();
    }

    /// Nothing may be left behind for the next run to trip over.
    #[test]
    fn replace_binary_leaves_no_staged_file_on_success() {
        let dir = unique_temp_dir();
        let dest = dir.join("dbops");
        replace_binary(&dest, b"new").unwrap();

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "dbops")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replace_binary_into_an_unwritable_directory_says_how_to_fix_it() {
        let dir = unique_temp_dir();
        let dest = dir.join("dbops");
        fs::write(&dest, b"old").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

        // Directory permissions don't apply to root, so there is no failure
        // to assert on there. Probe for that instead of assuming a uid.
        if fs::write(dir.join(".root-probe"), b"").is_ok() {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::remove_dir_all(&dir).ok();
            return;
        }

        let err = replace_binary(&dest, b"new").unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("sudo dbops update"), "got: {chain}");
        // The old binary must survive a failed update untouched.
        assert_eq!(fs::read(&dest).unwrap(), b"old");

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&dir).ok();
    }
}
