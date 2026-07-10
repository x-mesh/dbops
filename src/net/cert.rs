//! Minimal, self-contained X.509 DER reader used only to pull the
//! `notAfter` field out of a TLS peer certificate for `http check`'s
//! expiry check (PRD R31). Deliberately not a general certificate parser:
//! Cargo.toml is off-limits for this task, so this walks just enough of
//! RFC 5280's `Certificate { TBSCertificate { ..., validity, ... } }`
//! structure by hand instead of pulling in an x509 crate.

use std::time::{SystemTime, UNIX_EPOCH};

const TAG_SEQUENCE: u8 = 0x30;
const TAG_VERSION: u8 = 0xA0; // [0] EXPLICIT, context-specific + constructed
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;

/// One DER TLV: tag byte, its content, and whatever follows it. DER only
/// ever uses definite-form lengths (no BER indefinite-length marker), so
/// this doesn't need to handle that case.
fn read_tlv(buf: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let &tag = buf.first()?;
    let len_byte = *buf.get(1)?;
    let (len, header_len) = if len_byte & 0x80 == 0 {
        (len_byte as usize, 2usize)
    } else {
        let n = (len_byte & 0x7f) as usize;
        // Reject indefinite-length (n == 0, BER-only) and anything wider
        // than a leaf certificate could plausibly need.
        if n == 0 || n > 4 {
            return None;
        }
        let len_bytes = buf.get(2..2 + n)?;
        let len = len_bytes.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize);
        (len, 2 + n)
    };
    let content = buf.get(header_len..header_len + len)?;
    let rest = &buf[header_len + len..];
    Some((tag, content, rest))
}

/// Extract the certificate's `notAfter` timestamp (as Unix seconds) from a
/// DER-encoded leaf certificate. Returns `None` on anything unexpected
/// rather than guessing — callers treat a `None` as "cert expiry
/// unavailable", not as a hard failure of the surrounding `http check`.
pub fn parse_not_after(der: &[u8]) -> Option<i64> {
    let (tag, cert_content, _) = read_tlv(der)?;
    if tag != TAG_SEQUENCE {
        return None;
    }
    let (tag, tbs_content, _) = read_tlv(cert_content)?;
    if tag != TAG_SEQUENCE {
        return None;
    }

    // TBSCertificate ::= SEQUENCE {
    //   version [0] EXPLICIT INTEGER DEFAULT v1, serialNumber INTEGER,
    //   signature AlgorithmIdentifier, issuer Name, validity Validity, ... }
    let mut rest = tbs_content;
    let (tag, _version, next) = read_tlv(rest)?;
    if tag == TAG_VERSION {
        rest = next;
    }
    let (_tag, _serial, next) = read_tlv(rest)?; // serialNumber
    let (_tag, _sig_alg, next) = read_tlv(next)?; // signature AlgorithmIdentifier
    let (_tag, _issuer, next) = read_tlv(next)?; // issuer Name
    let (tag, validity, _next) = read_tlv(next)?; // validity SEQUENCE
    if tag != TAG_SEQUENCE {
        return None;
    }

    // Validity ::= SEQUENCE { notBefore Time, notAfter Time }
    let (_tag, _not_before, after_not_before) = read_tlv(validity)?;
    let (tag, content, _) = read_tlv(after_not_before)?;
    let (year, month, day, hour, min, sec) = match tag {
        TAG_UTC_TIME => parse_utc_time(content)?,
        TAG_GENERALIZED_TIME => parse_generalized_time(content)?,
        _ => return None,
    };
    to_unix_seconds(year, month, day, hour, min, sec)
}

/// `YYMMDDHHMMSSZ`. RFC 5280 4.1.2.5.1: two-digit years `< 50` mean 20xx,
/// otherwise 19xx (UTCTime only appears on certs dated before 2050).
fn parse_utc_time(bytes: &[u8]) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let s = std::str::from_utf8(bytes).ok()?.strip_suffix('Z')?;
    if s.len() != 12 {
        return None;
    }
    let yy: i32 = s.get(0..2)?.parse().ok()?;
    let year = if yy < 50 { 2000 + yy } else { 1900 + yy };
    Some((
        year,
        s.get(2..4)?.parse().ok()?,
        s.get(4..6)?.parse().ok()?,
        s.get(6..8)?.parse().ok()?,
        s.get(8..10)?.parse().ok()?,
        s.get(10..12)?.parse().ok()?,
    ))
}

/// `YYYYMMDDHHMMSSZ` (used once UTCTime's two-digit year runs out, i.e.
/// certs valid past 2049).
fn parse_generalized_time(bytes: &[u8]) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let s = std::str::from_utf8(bytes).ok()?.strip_suffix('Z')?;
    if s.len() != 14 {
        return None;
    }
    Some((
        s.get(0..4)?.parse().ok()?,
        s.get(4..6)?.parse().ok()?,
        s.get(6..8)?.parse().ok()?,
        s.get(8..10)?.parse().ok()?,
        s.get(10..12)?.parse().ok()?,
        s.get(12..14)?.parse().ok()?,
    ))
}

fn to_unix_seconds(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(i64::from(year), month, day);
    Some(days * 86_400 + i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec))
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a
/// proleptic-Gregorian civil date.
/// <http://howardhinnant.github.io/date_algorithms.html#days_from_civil>
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (i64::from(m) + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Whole days between now and `not_after_unix` (negative if already
/// expired).
pub fn days_until(not_after_unix: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (not_after_unix - now).div_euclid(86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_from_civil_matches_known_reference_points() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2020, 1, 1), 18_262);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782); // leap day
    }

    #[test]
    fn utc_time_pivots_at_the_rfc_5280_boundary() {
        assert_eq!(parse_utc_time(b"300101000000Z").unwrap().0, 2030);
        assert_eq!(parse_utc_time(b"500101000000Z").unwrap().0, 1950);
    }

    #[test]
    fn generalized_time_parses_four_digit_years() {
        assert_eq!(
            parse_generalized_time(b"20991231235959Z").unwrap(),
            (2099, 12, 31, 23, 59, 59)
        );
    }

    #[test]
    fn to_unix_seconds_matches_a_known_epoch_value() {
        // 2030-01-01T00:00:00Z
        assert_eq!(to_unix_seconds(2030, 1, 1, 0, 0, 0), Some(1_893_456_000));
    }

    #[test]
    fn parse_not_after_reads_a_synthetic_certificate() {
        let der = build_synthetic_certificate();
        let not_after = parse_not_after(&der).expect("parses notAfter");
        assert_eq!(not_after, to_unix_seconds(2030, 6, 15, 12, 0, 0).unwrap());
    }

    #[test]
    fn parse_not_after_rejects_garbage() {
        assert_eq!(parse_not_after(&[0x02, 0x01, 0x05]), None);
        assert_eq!(parse_not_after(&[]), None);
    }

    /// Builds a minimal DER `Certificate` shaped like RFC 5280 well enough
    /// for [`parse_not_after`] to walk: version, serial, sig-alg, issuer
    /// (all empty placeholders), then a real `validity` block.
    fn build_synthetic_certificate() -> Vec<u8> {
        fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
            let mut out = vec![tag, content.len() as u8];
            out.extend_from_slice(content);
            out
        }
        let not_before = tlv(TAG_UTC_TIME, b"250101000000Z");
        let not_after = tlv(TAG_UTC_TIME, b"300615120000Z");
        let mut validity_content = not_before;
        validity_content.extend(not_after);
        let validity = tlv(TAG_SEQUENCE, &validity_content);

        let version = tlv(TAG_VERSION, &[0x02, 0x01, 0x02]); // INTEGER 2 (v3)
        let serial = tlv(0x02, &[0x01]);
        let sig_alg = tlv(TAG_SEQUENCE, &[]);
        let issuer = tlv(TAG_SEQUENCE, &[]);

        let mut tbs_content = version;
        tbs_content.extend(serial);
        tbs_content.extend(sig_alg);
        tbs_content.extend(issuer);
        tbs_content.extend(validity);
        let tbs = tlv(TAG_SEQUENCE, &tbs_content);

        tlv(TAG_SEQUENCE, &tbs)
    }
}
