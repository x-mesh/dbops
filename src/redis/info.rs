//! Pure parser for the Redis `INFO` command's line-oriented text reply.
//!
//! Kept independent of any redis-crate connection types so `stats`,
//! `keyspace`, and `replication` can all unit test their report-building
//! logic against fixed text fixtures without a live server.

use std::collections::HashMap;

/// Parse `INFO` output into a flat key -> value map. Section headers
/// (`# Memory`) and blank lines are skipped; every other non-empty line is
/// split on the first `:`.
pub fn parse_info(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}

/// Parse a comma-separated `field=value` fragment, as used in `INFO`'s
/// `dbN:` and `slaveN:` line values (e.g. `keys=5,expires=1,avg_ttl=0`).
pub fn parse_fields(fragment: &str) -> HashMap<String, String> {
    fragment
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMORY_FIXTURE: &str = "\
# Memory
used_memory:1048576
used_memory_human:1.00M
maxmemory:0
maxmemory_policy:noeviction
mem_fragmentation_ratio:1.05
evicted_keys:0
";

    #[test]
    fn parses_key_value_lines() {
        let map = parse_info(MEMORY_FIXTURE);
        assert_eq!(map.get("used_memory").map(String::as_str), Some("1048576"));
        assert_eq!(map.get("mem_fragmentation_ratio").map(String::as_str), Some("1.05"));
        assert_eq!(map.get("evicted_keys").map(String::as_str), Some("0"));
    }

    #[test]
    fn skips_section_headers_and_blank_lines() {
        let map = parse_info(MEMORY_FIXTURE);
        assert_eq!(map.len(), 6);
    }

    #[test]
    fn parses_comma_separated_fields() {
        let fields = parse_fields("keys=5,expires=1,avg_ttl=0,subexpiry=0");
        assert_eq!(fields.get("keys").map(String::as_str), Some("5"));
        assert_eq!(fields.get("expires").map(String::as_str), Some("1"));
        assert_eq!(fields.get("avg_ttl").map(String::as_str), Some("0"));
    }

    #[test]
    fn empty_input_yields_empty_map() {
        assert!(parse_info("").is_empty());
        assert!(parse_fields("").is_empty());
    }
}
