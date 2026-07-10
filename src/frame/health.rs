use std::time::Duration;

use anyhow::{bail, Result};
use clap::Args;

/// Shared threshold flags for every `health` subcommand across domains.
#[derive(Args, Debug)]
pub struct HealthArgs {
    #[arg(long, value_name = "VAL")]
    pub warning: Option<String>,

    #[arg(long, value_name = "VAL")]
    pub critical: Option<String>,
}

/// A parsed `--warning`/`--critical` value, before any domain decides what
/// it means. `Duration` comes from a recognized time suffix (`ms`, `s`,
/// `m`); `Count` comes from a bare number with no suffix and is left for the
/// calling domain to interpret -- as seconds (`pg`/`mongo` replication lag),
/// milliseconds (`redis` response time), or a plain count (`os` unassigned
/// shards).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Threshold {
    Duration(Duration),
    Count(f64),
}

impl Threshold {
    /// Interpret this threshold as seconds: a `Duration` measured directly,
    /// or a bare `Count` treated as a plain number of seconds -- the legacy
    /// `--warning 10` == "10 seconds" convention `pg`/`mongo` lag checks use.
    pub fn as_seconds(self) -> f64 {
        match self {
            Threshold::Duration(d) => d.as_secs_f64(),
            Threshold::Count(n) => n,
        }
    }

    /// Interpret this threshold as milliseconds: a `Duration` converted to
    /// ms, or a bare `Count` treated as a plain number of milliseconds --
    /// the legacy `--warning 50` == "50ms" convention `redis`'s
    /// response-time check uses.
    pub fn as_millis_f64(self) -> f64 {
        match self {
            Threshold::Duration(d) => d.as_secs_f64() * 1_000.0,
            Threshold::Count(n) => n,
        }
    }

    /// Interpret this threshold as a plain count. Only a bare `Count` is
    /// valid here -- a `Duration` (`"5s"`, `"500ms"`, ...) is a usage error
    /// because the domain measures a count, not time.
    pub fn as_count(self) -> Result<f64> {
        match self {
            Threshold::Count(n) => Ok(n),
            Threshold::Duration(_) => {
                bail!("expected a plain count (no time unit like ms/s/m)")
            }
        }
    }
}

/// Parse a `--warning`/`--critical` threshold shared by every `health`
/// subcommand. Unlike [`crate::frame::ctx::parse_timeout`] (which powers
/// `--timeout` and forbids `0` -- a zero timeout is nonsensical) `0` is a
/// perfectly ordinary threshold here, e.g. `--critical 0s` to alert on *any*
/// replication lag at all. A recognized duration suffix (`ms`, `s`, `m`)
/// parses as [`Threshold::Duration`]; a bare number with no suffix parses as
/// [`Threshold::Count`], left for the caller to interpret. Anything else --
/// garbage text, a negative number, an empty string -- is a usage error.
pub fn parse_threshold(raw: &str) -> Result<Threshold> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("expected a threshold value, got an empty string");
    }

    // "ms" is checked before "s" so "500ms" doesn't get misread as "500m"
    // with a dangling "s".
    let (digits, unit_ms): (&str, Option<f64>) = if let Some(v) = trimmed.strip_suffix("ms") {
        (v, Some(1.0))
    } else if let Some(v) = trimmed.strip_suffix('s') {
        (v, Some(1_000.0))
    } else if let Some(v) = trimmed.strip_suffix('m') {
        (v, Some(60_000.0))
    } else {
        (trimmed, None)
    };

    let n: f64 = digits.trim().parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid threshold {raw:?}: expected a number, or a duration like 500ms/5s/2m"
        )
    })?;
    if !n.is_finite() || n < 0.0 {
        bail!("invalid threshold {raw:?}: expected a non-negative number");
    }

    Ok(match unit_ms {
        Some(factor) => Threshold::Duration(Duration::from_secs_f64(n * factor / 1_000.0)),
        None => Threshold::Count(n),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_allowed_in_every_form() {
        assert_eq!(parse_threshold("0").unwrap(), Threshold::Count(0.0));
        assert_eq!(
            parse_threshold("0s").unwrap(),
            Threshold::Duration(Duration::ZERO)
        );
        assert_eq!(
            parse_threshold("0ms").unwrap(),
            Threshold::Duration(Duration::ZERO)
        );
        assert_eq!(
            parse_threshold("0m").unwrap(),
            Threshold::Duration(Duration::ZERO)
        );
    }

    #[test]
    fn recognized_suffixes_parse_as_duration() {
        assert_eq!(
            parse_threshold("500ms").unwrap(),
            Threshold::Duration(Duration::from_millis(500))
        );
        assert_eq!(
            parse_threshold("5s").unwrap(),
            Threshold::Duration(Duration::from_secs(5))
        );
        assert_eq!(
            parse_threshold("2m").unwrap(),
            Threshold::Duration(Duration::from_secs(120))
        );
    }

    #[test]
    fn bare_number_parses_as_count() {
        assert_eq!(parse_threshold("7").unwrap(), Threshold::Count(7.0));
        assert_eq!(parse_threshold("2.5").unwrap(), Threshold::Count(2.5));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_threshold("abc").is_err());
        assert!(parse_threshold("").is_err());
        assert!(parse_threshold("   ").is_err());
        assert!(parse_threshold("s").is_err());
        assert!(parse_threshold("ms").is_err());
    }

    #[test]
    fn negative_numbers_are_rejected() {
        assert!(parse_threshold("-1").is_err());
        assert!(parse_threshold("-5s").is_err());
    }

    #[test]
    fn as_seconds_treats_count_as_seconds() {
        assert_eq!(Threshold::Count(10.0).as_seconds(), 10.0);
        assert_eq!(
            Threshold::Duration(Duration::from_millis(500)).as_seconds(),
            0.5
        );
    }

    #[test]
    fn as_millis_treats_count_as_milliseconds() {
        assert_eq!(Threshold::Count(50.0).as_millis_f64(), 50.0);
        assert_eq!(
            Threshold::Duration(Duration::from_secs(1)).as_millis_f64(),
            1000.0
        );
    }

    #[test]
    fn as_count_accepts_count_and_rejects_duration() {
        assert_eq!(Threshold::Count(3.0).as_count().unwrap(), 3.0);
        assert!(Threshold::Duration(Duration::from_secs(1))
            .as_count()
            .is_err());
    }
}
