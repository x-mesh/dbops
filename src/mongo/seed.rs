//! `dbops mongo seed --collection <db.collection> --file <path>`.
//!
//! Two input formats, auto-detected from the first non-whitespace character
//! of the file:
//! - **NDJSON** (one JSON document per line, the default/recommended form):
//!   read via a `BufReader`, one line at a time, batched into 1000-document
//!   `insertMany(ordered=false)` chunks. Memory use stays flat regardless of
//!   file size. This is what makes multi-GB seed files safe (PRD R16 edge
//!   case).
//! - **JSON array** (`[ {...}, {...} ]`): a streaming array parser is more
//!   machinery than this tool needs, so instead the file is size-capped at
//!   [`MAX_ARRAY_BYTES`] and loaded whole. Prefer NDJSON for anything larger.
//!
//! `ordered=false` means one bad document doesn't abort documents after it
//! in the same chunk, but a failure is still fail-fast at the *seed*
//! level: the first bad line/document (whether a JSON parse error or a
//! server-side write error) stops the whole run and is reported as "N
//! documents inserted, then failed at line/document M: <reason>", so a
//! partial run is never silently ambiguous about how far it got (PRD R16
//! edge case).
//!
//! Like [`super::init`], the target is planned first ([`PlanPreview`],
//! probed for real: the source file is read once up front purely to count
//! documents so `--dry-run` reports an accurate estimate) and only inserted
//! into once [`guard::authorize`] returns
//! [`GuardDecision::Proceed`].

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use mongodb::bson::Document;
use mongodb::error::ErrorKind;
use mongodb::Collection;

use crate::frame::exit::unix;
use crate::frame::guard::{self, GuardDecision};
use crate::frame::plan::{ActionKind, PlanPreview, PlannedAction};
use crate::frame::{Ctx, ExitCode};
use crate::mongo::client;

/// Documents per `insertMany` call. Small enough to keep a single failed
/// batch's blast radius (and the BSON wire message size) bounded, large
/// enough that a 100k-line file doesn't need 100k round trips.
const CHUNK_SIZE: usize = 1000;

/// JSON array seed files larger than this are rejected with a message
/// pointing at NDJSON instead. See the module doc comment for why array
/// files are loaded whole rather than streamed.
const MAX_ARRAY_BYTES: u64 = 50 * 1024 * 1024;

pub async fn run(
    ctx: &Ctx,
    collection: &str,
    db_flag: Option<&str>,
    file: &Path,
    confirm_name: Option<&str>,
) -> Result<ExitCode> {
    let (db_name, coll_name) = match resolve_target(collection, db_flag) {
        Ok(target) => target,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
        }
    };

    let source = match probe_source(file) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::ARGUMENT_ERROR));
        }
    };

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Insert,
            target: format!("{db_name}.{coll_name}"),
            detail: format!(
                "insert seed documents from {} ({})",
                file.display(),
                source.mode.label(),
            ),
            estimated_records: source.estimated_docs,
        }],
    };

    let mongo_client = match client::connect(&ctx.profile.mongodb, ctx.timeout, ctx.insecure).await
    {
        Ok(mongo_client) => mongo_client,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(unix::CONNECTION_FAILED));
        }
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => Ok(ExitCode::from(unix::SUCCESS)),
        GuardDecision::Declined => Ok(ExitCode::from(unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {
            let collection = mongo_client
                .database(&db_name)
                .collection::<Document>(&coll_name);
            let outcome = match source.mode {
                SourceMode::Ndjson => insert_ndjson(&collection, file, ctx.timeout).await?,
                SourceMode::JsonArray(docs) => {
                    insert_json_array(&collection, docs, ctx.timeout).await?
                }
            };
            Ok(report_outcome(&outcome))
        }
    }
}

/// `--collection` is either `"db.collection"` on its own, or a bare
/// collection name that must be paired with `--db`. Rejecting a bare name
/// without `--db` (rather than guessing a default database) matches R7/R15's
/// "never guess a destructive target" rule, extended here to a mutating one.
fn resolve_target(collection: &str, db_flag: Option<&str>) -> Result<(String, String)> {
    match collection.split_once('.') {
        Some((db_part, coll_part)) if !db_part.is_empty() && !coll_part.is_empty() => {
            if let Some(db_flag) = db_flag {
                if db_flag != db_part {
                    bail!(
                        "--collection {collection:?} already specifies database '{db_part}', \
                         which conflicts with --db {db_flag:?}"
                    );
                }
            }
            Ok((db_part.to_string(), coll_part.to_string()))
        }
        _ => {
            let db = db_flag
                .context("--collection is a bare name; pass --db <name> or use 'db.collection'")?;
            Ok((db.to_string(), collection.to_string()))
        }
    }
}

// --- source probing (dry-run-safe: read-only) -------------------------------

enum SourceMode {
    Ndjson,
    JsonArray(Vec<Document>),
}

impl SourceMode {
    fn label(&self) -> &'static str {
        match self {
            SourceMode::Ndjson => "NDJSON, streamed",
            SourceMode::JsonArray(_) => "JSON array, loaded whole",
        }
    }
}

struct SeedSource {
    mode: SourceMode,
    estimated_docs: Option<u64>,
}

/// Detect NDJSON vs. JSON-array (first non-whitespace character `[`) and, in
/// the same pass, count how many documents will actually be inserted. Run
/// unconditionally (even under `--dry-run`) so the plan preview reflects the
/// real file rather than a guess.
fn probe_source(file: &Path) -> Result<SeedSource> {
    let opened = File::open(file)
        .with_context(|| format!("failed to open seed file: {}", file.display()))?;
    let mut reader = BufReader::new(opened);
    let mut first_line = String::new();
    let first_len = reader
        .read_line(&mut first_line)
        .with_context(|| format!("failed to read seed file: {}", file.display()))?;

    if first_len == 0 {
        return Ok(SeedSource {
            mode: SourceMode::Ndjson,
            estimated_docs: Some(0),
        });
    }

    if first_line.trim_start().starts_with('[') {
        let meta = std::fs::metadata(file)
            .with_context(|| format!("failed to stat seed file: {}", file.display()))?;
        if meta.len() > MAX_ARRAY_BYTES {
            bail!(
                "JSON array seed files are limited to {MAX_ARRAY_BYTES} bytes ({} given); convert \
                 to NDJSON (one document per line), which streams with no size limit",
                meta.len()
            );
        }
        let content = std::fs::read_to_string(file)
            .with_context(|| format!("failed to read seed file: {}", file.display()))?;
        let docs: Vec<Document> = serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON array in {}", file.display()))?;
        let count = docs.len() as u64;
        return Ok(SeedSource {
            mode: SourceMode::JsonArray(docs),
            estimated_docs: Some(count),
        });
    }

    // NDJSON: count non-blank lines across the rest of the file. Uses
    // `read_until` (not `lines()`/`BufRead::lines`) so a single very long or
    // non-UTF8 line can't grow unboundedly retained memory or abort the
    // count early. This pass only needs to know where lines end, not
    // decode them.
    let mut count: u64 = u64::from(!first_line.trim().is_empty());
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("failed to read seed file: {}", file.display()))?;
        if n == 0 {
            break;
        }
        if !buf.iter().all(u8::is_ascii_whitespace) {
            count += 1;
        }
    }

    Ok(SeedSource {
        mode: SourceMode::Ndjson,
        estimated_docs: Some(count),
    })
}

// --- insertion ---------------------------------------------------------------

struct SeedOutcome {
    succeeded: u64,
    failed_at: Option<FailurePoint>,
}

struct FailurePoint {
    /// 1-based NDJSON line number, or 1-based JSON-array element index.
    position: u64,
    message: String,
}

async fn insert_ndjson(
    collection: &Collection<Document>,
    file: &Path,
    timeout: Duration,
) -> Result<SeedOutcome> {
    let opened = File::open(file)
        .with_context(|| format!("failed to open seed file: {}", file.display()))?;
    let mut lines = BufReader::new(opened).lines();

    let mut succeeded: u64 = 0;
    let mut chunk: Vec<(u64, Document)> = Vec::with_capacity(CHUNK_SIZE);
    let mut line_no: u64 = 0;

    loop {
        let next = lines.next();
        line_no += 1;
        let raw_line = match next {
            None => break,
            Some(Ok(raw_line)) => raw_line,
            Some(Err(err)) => {
                let (n, early_failure) = flush_chunk(collection, &mut chunk, timeout).await?;
                succeeded += n;
                return Ok(SeedOutcome {
                    succeeded,
                    failed_at: early_failure.or(Some(FailurePoint {
                        position: line_no,
                        message: format!("failed to read line: {err}"),
                    })),
                });
            }
        };

        match parse_ndjson_line(&raw_line) {
            ParsedLine::Blank => continue,
            ParsedLine::Doc(doc) => {
                chunk.push((line_no, doc));
                if chunk.len() >= CHUNK_SIZE {
                    let (n, failure) = flush_chunk(collection, &mut chunk, timeout).await?;
                    succeeded += n;
                    if let Some(failure) = failure {
                        return Ok(SeedOutcome {
                            succeeded,
                            failed_at: Some(failure),
                        });
                    }
                }
            }
            ParsedLine::Invalid(message) => {
                let (n, early_failure) = flush_chunk(collection, &mut chunk, timeout).await?;
                succeeded += n;
                return Ok(SeedOutcome {
                    succeeded,
                    failed_at: early_failure.or(Some(FailurePoint {
                        position: line_no,
                        message,
                    })),
                });
            }
        }
    }

    let (n, failure) = flush_chunk(collection, &mut chunk, timeout).await?;
    succeeded += n;
    Ok(SeedOutcome {
        succeeded,
        failed_at: failure,
    })
}

async fn insert_json_array(
    collection: &Collection<Document>,
    docs: Vec<Document>,
    timeout: Duration,
) -> Result<SeedOutcome> {
    let mut succeeded: u64 = 0;
    for (chunk_idx, chunk_slice) in docs.chunks(CHUNK_SIZE).enumerate() {
        let mut chunk: Vec<(u64, Document)> = chunk_slice
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, doc)| ((chunk_idx * CHUNK_SIZE + i + 1) as u64, doc))
            .collect();
        let (n, failure) = flush_chunk(collection, &mut chunk, timeout).await?;
        succeeded += n;
        if let Some(failure) = failure {
            return Ok(SeedOutcome {
                succeeded,
                failed_at: Some(failure),
            });
        }
    }
    Ok(SeedOutcome {
        succeeded,
        failed_at: None,
    })
}

enum ParsedLine {
    Blank,
    Doc(Document),
    Invalid(String),
}

/// Pure: one raw NDJSON line -> what it means for the seed run. Blank/
/// whitespace-only lines are silently skipped (common NDJSON convention,
/// e.g. a trailing newline at EOF) rather than treated as corruption.
fn parse_ndjson_line(raw_line: &str) -> ParsedLine {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() {
        return ParsedLine::Blank;
    }
    match serde_json::from_str::<Document>(trimmed) {
        Ok(doc) => ParsedLine::Doc(doc),
        Err(err) => ParsedLine::Invalid(format!("invalid JSON: {err}")),
    }
}

/// Insert one already-parsed chunk with `ordered=false` and clear it.
/// Returns how many documents actually made it in and, if any didn't, the
/// first failure. `entries` carries each document's original line/index so
/// a `write_errors[i].index` (position within *this chunk*) maps back to the
/// right place in the source file even when earlier lines in the same chunk
/// were blank and never got pushed.
async fn flush_chunk(
    collection: &Collection<Document>,
    entries: &mut Vec<(u64, Document)>,
    timeout: Duration,
) -> Result<(u64, Option<FailurePoint>)> {
    if entries.is_empty() {
        return Ok((0, None));
    }
    let taken = std::mem::take(entries);
    let attempted = taken.len() as u64;
    let docs: Vec<&Document> = taken.iter().map(|(_, doc)| doc).collect();

    let result = tokio::time::timeout(timeout, collection.insert_many(docs).ordered(false)).await;

    match result {
        Err(_elapsed) => bail!("insertMany timed out after {timeout:?}"),
        Ok(Ok(_)) => Ok((attempted, None)),
        Ok(Err(err)) => match err.kind.as_ref() {
            ErrorKind::InsertMany(insert_err) => {
                let failed_count = insert_err
                    .write_errors
                    .as_ref()
                    .map_or(0, |errors| errors.len() as u64);
                let succeeded_here = attempted.saturating_sub(failed_count);
                let first_error = insert_err
                    .write_errors
                    .as_ref()
                    .and_then(|errors| errors.iter().min_by_key(|e| e.index));
                let failure = match first_error {
                    Some(e) => {
                        let position = taken.get(e.index).map_or(taken[0].0, |(line, _)| *line);
                        FailurePoint {
                            position,
                            message: e.message.clone(),
                        }
                    }
                    None => FailurePoint {
                        position: taken[0].0,
                        message: format!("{err:#}"),
                    },
                };
                Ok((succeeded_here, Some(failure)))
            }
            _ => Ok((
                0,
                Some(FailurePoint {
                    position: taken[0].0,
                    message: format!("{err:#}"),
                }),
            )),
        },
    }
}

fn report_outcome(outcome: &SeedOutcome) -> ExitCode {
    let plural = if outcome.succeeded == 1 { "" } else { "s" };
    match &outcome.failed_at {
        None => {
            println!(
                "seed complete: {} document{plural} inserted",
                outcome.succeeded
            );
            ExitCode::from(unix::SUCCESS)
        }
        Some(failure) => {
            eprintln!(
                "error: {} document{plural} inserted, then failed at line/document {}: {}",
                outcome.succeeded, failure.position, failure.message
            );
            ExitCode::from(unix::GENERAL_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- resolve_target ----------------------------------------------------------

    #[test]
    fn resolve_target_accepts_dotted_collection_alone() {
        let (db, coll) = resolve_target("app.users", None).unwrap();
        assert_eq!(db, "app");
        assert_eq!(coll, "users");
    }

    #[test]
    fn resolve_target_accepts_bare_collection_with_db_flag() {
        let (db, coll) = resolve_target("users", Some("app")).unwrap();
        assert_eq!(db, "app");
        assert_eq!(coll, "users");
    }

    #[test]
    fn resolve_target_accepts_matching_dotted_and_db_flag() {
        let (db, coll) = resolve_target("app.users", Some("app")).unwrap();
        assert_eq!(db, "app");
        assert_eq!(coll, "users");
    }

    #[test]
    fn resolve_target_rejects_conflicting_db_flag() {
        let err = resolve_target("app.users", Some("other")).unwrap_err();
        assert!(err.to_string().contains("conflicts"));
    }

    #[test]
    fn resolve_target_rejects_bare_collection_without_db_flag() {
        let err = resolve_target("users", None).unwrap_err();
        assert!(err.to_string().contains("--db"));
    }

    #[test]
    fn resolve_target_treats_a_leading_or_trailing_dot_as_a_bare_name() {
        // ".users" / "app." have an empty db-or-collection half, so they
        // fall into the "bare name, needs --db" branch rather than being
        // silently accepted with an empty database name.
        assert!(resolve_target(".users", None).is_err());
        assert!(resolve_target("app.", None).is_err());
    }

    // --- parse_ndjson_line -------------------------------------------------------

    #[test]
    fn parse_ndjson_line_reads_a_valid_document() {
        match parse_ndjson_line(r#"{"a": 1, "b": "x"}"#) {
            ParsedLine::Doc(doc) => {
                assert_eq!(doc.get_i32("a").unwrap(), 1);
                assert_eq!(doc.get_str("b").unwrap(), "x");
            }
            _ => panic!("expected Doc"),
        }
    }

    #[test]
    fn parse_ndjson_line_treats_blank_and_whitespace_only_lines_as_blank() {
        assert!(matches!(parse_ndjson_line(""), ParsedLine::Blank));
        assert!(matches!(parse_ndjson_line("   \t  "), ParsedLine::Blank));
    }

    #[test]
    fn parse_ndjson_line_reports_invalid_json() {
        match parse_ndjson_line("{not json}") {
            ParsedLine::Invalid(msg) => assert!(msg.contains("invalid JSON")),
            _ => panic!("expected Invalid"),
        }
    }

    // --- report_outcome ------------------------------------------------------------

    #[test]
    fn report_outcome_success_is_exit_zero() {
        let outcome = SeedOutcome {
            succeeded: 5,
            failed_at: None,
        };
        assert_eq!(report_outcome(&outcome), ExitCode::from(unix::SUCCESS));
    }

    #[test]
    fn report_outcome_failure_is_general_error() {
        let outcome = SeedOutcome {
            succeeded: 3,
            failed_at: Some(FailurePoint {
                position: 4,
                message: "bad".to_string(),
            }),
        };
        assert_eq!(
            report_outcome(&outcome),
            ExitCode::from(unix::GENERAL_ERROR)
        );
    }

    // --- probe_source: NDJSON counting/mode-detection, no network needed ---------

    #[test]
    fn probe_source_counts_ndjson_lines_and_skips_blanks() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("dbops-seed-test-{}.ndjson", std::process::id()));
        std::fs::write(&path, "{\"a\":1}\n\n{\"a\":2}\n   \n{\"a\":3}\n").unwrap();

        let source = probe_source(&path).unwrap();
        assert!(matches!(source.mode, SourceMode::Ndjson));
        assert_eq!(source.estimated_docs, Some(3));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn probe_source_detects_a_json_array_and_counts_its_elements() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("dbops-seed-test-array-{}.json", std::process::id()));
        std::fs::write(&path, "[{\"a\":1}, {\"a\":2}]").unwrap();

        let source = probe_source(&path).unwrap();
        match source.mode {
            SourceMode::JsonArray(docs) => assert_eq!(docs.len(), 2),
            SourceMode::Ndjson => panic!("expected JsonArray"),
        }
        assert_eq!(source.estimated_docs, Some(2));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn probe_source_empty_file_is_zero_documents() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "dbops-seed-test-empty-{}.ndjson",
            std::process::id()
        ));
        std::fs::write(&path, "").unwrap();

        let source = probe_source(&path).unwrap();
        assert_eq!(source.estimated_docs, Some(0));

        std::fs::remove_file(path).ok();
    }
}
