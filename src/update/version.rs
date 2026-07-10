//! Just enough semver to answer one question: is the published release tag
//! newer than the version compiled into this binary?
//!
//! Not a general semver implementation. Build metadata (`+abc`) is dropped
//! because the spec says it never affects precedence, and a pre-release
//! suffix is compared as one opaque string instead of dot-segment by
//! dot-segment. Both simplifications are safe for a comparison between two
//! tags of the same project, and neither can make an older tag look newer.

use std::cmp::Ordering;
use std::fmt;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `major.minor.patch`, zero-padded if the tag omitted a component.
    core: [u64; 3],
    /// `None` for a final release. Per semver, any pre-release sorts *below*
    /// the same core version without one: `1.0.0-rc.1` < `1.0.0`.
    pre: Option<String>,
}

/// Parse `0.1.0`, `v0.1.0`, `v1.2`, `0.2.0-rc.1`, or `0.2.0+build.5`.
pub fn parse(raw: &str) -> Result<Version> {
    let trimmed = raw.trim();
    let without_v = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let without_build = without_v.split('+').next().unwrap_or(without_v);

    let (core_str, pre) = match without_build.split_once('-') {
        Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
        _ => (without_build, None),
    };

    let parts: Vec<&str> = core_str.split('.').collect();
    if parts.len() > 3 {
        bail!("invalid version {raw:?}: more than three dot-separated components");
    }
    let mut core = [0u64; 3];
    for (slot, part) in core.iter_mut().zip(parts) {
        *slot = part
            .parse()
            .with_context(|| format!("invalid version {raw:?}: {part:?} is not a number"))?;
    }

    Ok(Version { core, pre })
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                // A final release outranks any pre-release of the same core.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Always three components, `v`-less — which is exactly the spelling the
/// release artifacts use in their file names.
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.core;
        write!(f, "{major}.{minor}.{patch}")?;
        match &self.pre {
            Some(pre) => write!(f, "-{pre}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(raw: &str) -> Version {
        parse(raw).unwrap()
    }

    #[test]
    fn accepts_the_tag_and_cargo_spellings_of_one_version() {
        assert_eq!(v("v0.1.0"), v("0.1.0"));
        assert_eq!(v(" v0.1.0 "), v("0.1.0"));
    }

    #[test]
    fn missing_components_are_zero_padded() {
        assert_eq!(v("v1.2"), v("1.2.0"));
        assert_eq!(v("1"), v("1.0.0"));
    }

    #[test]
    fn build_metadata_never_affects_precedence() {
        assert_eq!(v("0.2.0+build.5"), v("0.2.0"));
        assert_eq!(v("0.2.0-rc.1+build.5"), v("0.2.0-rc.1"));
    }

    #[test]
    fn core_components_compare_numerically_not_lexically() {
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.1.10") > v("0.1.9"));
    }

    #[test]
    fn a_prerelease_sorts_below_its_final_release() {
        assert!(v("0.2.0-rc.1") < v("0.2.0"));
        assert!(v("0.2.0-rc.1") > v("0.1.9"));
        assert!(v("0.2.0-rc.2") > v("0.2.0-rc.1"));
    }

    #[test]
    fn display_normalizes_to_the_artifact_naming() {
        assert_eq!(v("v1.2").to_string(), "1.2.0");
        assert_eq!(v("v0.2.0-rc.1").to_string(), "0.2.0-rc.1");
        assert_eq!(v("0.2.0+build.5").to_string(), "0.2.0");
    }

    #[test]
    fn the_compiled_in_cargo_version_always_parses() {
        assert!(parse(env!("CARGO_PKG_VERSION")).is_ok());
    }

    #[test]
    fn garbage_tags_are_rejected_rather_than_silently_zeroed() {
        assert!(parse("").is_err());
        assert!(parse("latest").is_err());
        assert!(parse("v1.2.3.4").is_err());
        assert!(parse("v1.x.0").is_err());
    }
}
