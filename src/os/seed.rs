//! `dbops os seed --index <name> --file <file>` -- streams an NDJSON file
//! (one JSON document per line) into `<name>` via `_bulk`, in fixed-size
//! chunks, without ever holding the whole file in memory.
//!
//! A corrupt line (fails to parse as JSON) stops the stream: whatever chunk
//! was already buffered is flushed first, so the reported success count
//! reflects documents actually sent, then the command reports the failing
//! line number and reason and exits non-zero. Per-item `_bulk` failures
//! (e.g. a mapping conflict on an otherwise well-formed document) are
//! aggregated separately and also fail the command, since a partial load
//! that looks like a full success is worse than a loud one.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context};
use opensearch::{BulkOperation, BulkParts, OpenSearch};
use serde_json::Value;

use crate::frame::exit;
use crate::frame::guard::{self, GuardDecision};
use crate::frame::plan::{ActionKind, PlanPreview, PlannedAction};
use crate::frame::{Ctx, ExitCode};

/// Documents per `_bulk` request. Bounds peak memory (a chunk of parsed
/// `Value`s plus its serialized request body) independent of file size.
const CHUNK_SIZE: usize = 1000;

pub async fn run_seed(
    index: &str,
    file: &Path,
    confirm_name: Option<&str>,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    let os_client = match super::client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure)
    {
        Ok(os_client) => os_client,
        Err(err) => {
            eprintln!("error: failed to connect to opensearch: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Insert,
            target: index.to_string(),
            detail: format!("bulk-index NDJSON documents from {}", file.display()),
            estimated_records: None,
        }],
    };

    match guard::authorize(ctx, &plan, confirm_name)? {
        GuardDecision::DryRun => return Ok(ExitCode::from(exit::unix::SUCCESS)),
        GuardDecision::Declined => return Ok(ExitCode::from(exit::unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => {}
    }

    match stream_seed(&os_client, ctx.timeout, index, file).await {
        Ok(outcome) if outcome.item_failures == 0 => {
            println!(
                "seed complete: {} document(s) indexed into '{index}'",
                outcome.indexed
            );
            Ok(ExitCode::from(exit::unix::SUCCESS))
        }
        Ok(outcome) => {
            eprintln!(
                "error: {}건 성공, {}건은 _bulk 개별 항목 오류로 실패",
                outcome.indexed, outcome.item_failures
            );
            Ok(ExitCode::from(exit::unix::GENERAL_ERROR))
        }
        Err(SeedError::LineParse {
            indexed,
            item_failures,
            line,
            source,
        }) => {
            eprintln!("error: {indexed}건 성공, {line}번째 줄에서 실패: {source}");
            if item_failures > 0 {
                eprintln!("error: 그 중 {item_failures}건은 _bulk 개별 항목 오류로도 실패");
            }
            Ok(ExitCode::from(exit::unix::GENERAL_ERROR))
        }
        Err(SeedError::Other(err)) => {
            eprintln!("error: {err:#}");
            Ok(ExitCode::from(exit::unix::GENERAL_ERROR))
        }
    }
}

struct SeedOutcome {
    indexed: u64,
    item_failures: u64,
}

enum SeedError {
    /// Line `line` (1-indexed) failed to parse as JSON. `indexed` is the
    /// count of documents already sent via `_bulk` before this point --
    /// always flushed before this variant is returned, so it's accurate.
    LineParse {
        indexed: u64,
        item_failures: u64,
        line: u64,
        source: String,
    },
    Other(anyhow::Error),
}

impl From<anyhow::Error> for SeedError {
    fn from(err: anyhow::Error) -> Self {
        SeedError::Other(err)
    }
}

async fn stream_seed(
    client: &OpenSearch,
    timeout: Duration,
    index: &str,
    file: &Path,
) -> Result<SeedOutcome, SeedError> {
    let f = File::open(file)
        .with_context(|| format!("failed to open seed file: {}", file.display()))?;
    let reader = BufReader::new(f);

    let mut indexed: u64 = 0;
    let mut item_failures: u64 = 0;
    let mut chunk: Vec<Value> = Vec::with_capacity(CHUNK_SIZE);

    for (i, line_result) in reader.lines().enumerate() {
        let line_no = i as u64 + 1;
        let line = line_result.with_context(|| format!("failed to read line {line_no}"))?;
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(parse_err) => {
                if !chunk.is_empty() {
                    let (sent, failed) =
                        send_chunk(client, timeout, index, std::mem::take(&mut chunk)).await?;
                    indexed += sent;
                    item_failures += failed;
                }
                return Err(SeedError::LineParse {
                    indexed,
                    item_failures,
                    line: line_no,
                    source: parse_err.to_string(),
                });
            }
        };
        chunk.push(value);

        if chunk.len() >= CHUNK_SIZE {
            let (sent, failed) =
                send_chunk(client, timeout, index, std::mem::take(&mut chunk)).await?;
            indexed += sent;
            item_failures += failed;
            eprintln!("progress: {indexed} document(s) indexed");
        }
    }

    if !chunk.is_empty() {
        let (sent, failed) = send_chunk(client, timeout, index, chunk).await?;
        indexed += sent;
        item_failures += failed;
    }

    Ok(SeedOutcome {
        indexed,
        item_failures,
    })
}

/// Send one `_bulk` request for `docs`. Returns `(succeeded, failed)`,
/// where `failed` comes from the response's per-item `error` fields (a
/// non-2xx HTTP status on the request itself is a hard `Err`, not folded
/// into this count).
async fn send_chunk(
    client: &OpenSearch,
    timeout: Duration,
    index: &str,
    docs: Vec<Value>,
) -> anyhow::Result<(u64, u64)> {
    let total = docs.len() as u64;
    let ops: Vec<BulkOperation<Value>> = docs
        .into_iter()
        .map(|doc| BulkOperation::index(doc).into())
        .collect();

    let fut = client.bulk(BulkParts::Index(index)).body(ops).send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("_bulk timed out")?
        .context("failed to call _bulk")?;

    let status = response.status_code();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        bail!("opensearch _bulk returned HTTP {status}: {body_text}");
    }

    let body: Value = response
        .json()
        .await
        .context("failed to parse _bulk response")?;
    let failed = count_item_failures(&body);
    Ok((total - failed, failed))
}

/// `_bulk` responses set a top-level `"errors": true` when any item failed;
/// each failed item carries an `"error"` object under its action key
/// (`"index"`, since every op here is an index action).
fn count_item_failures(body: &Value) -> u64 {
    let has_errors = body.get("errors").and_then(Value::as_bool).unwrap_or(false);
    if !has_errors {
        return 0;
    }
    body.get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.as_object()
                        .and_then(|obj| obj.values().next())
                        .and_then(|op| op.get("error"))
                        .is_some()
                })
                .count() as u64
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn count_item_failures_is_zero_when_errors_false() {
        let body = json!({"errors": false, "items": [{"index": {"status": 201}}]});
        assert_eq!(count_item_failures(&body), 0);
    }

    #[test]
    fn count_item_failures_counts_only_failed_items() {
        let body = json!({
            "errors": true,
            "items": [
                {"index": {"status": 201}},
                {"index": {"status": 409, "error": {"type": "version_conflict_engine_exception"}}},
                {"index": {"status": 201}},
            ]
        });
        assert_eq!(count_item_failures(&body), 1);
    }

    #[test]
    fn count_item_failures_handles_missing_items() {
        let body = json!({"errors": true});
        assert_eq!(count_item_failures(&body), 0);
    }
}
