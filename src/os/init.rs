//! `dbops os init index` -- idempotent index creation from a mapping file,
//! plus the shared `exists`/`create`/`get-mapping` helpers `super::run_reset`
//! reuses to recreate an index with its previous mapping.
//!
//! The mapping file's JSON is passed through as-is as the `PUT /<name>`
//! request body (typically `{"mappings": {...}}`, but a caller can also
//! include `"settings"`/`"aliases"` -- this module does not interpret it).

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use opensearch::http::response::Response;
use opensearch::indices::{IndicesCreateParts, IndicesExistsParts, IndicesGetMappingParts};
use opensearch::OpenSearch;
use serde_json::Value;

use crate::frame::exit;
use crate::frame::guard::{self, GuardDecision};
use crate::frame::plan::{ActionKind, PlanPreview, PlannedAction};
use crate::frame::{Ctx, ExitCode};

use super::OsInitTarget;

pub async fn run_init(target: &OsInitTarget, ctx: &Ctx) -> Result<ExitCode> {
    let OsInitTarget::Index {
        name,
        mapping,
        if_not_exists,
        confirm_name,
    } = target;

    let os_client = match super::client::connect(&ctx.profile.opensearch, ctx.timeout, ctx.insecure)
    {
        Ok(os_client) => os_client,
        Err(err) => {
            eprintln!("error: failed to connect to opensearch: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };

    let exists = match index_exists(&os_client, ctx.timeout, name).await {
        Ok(exists) => exists,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(exit::unix::CONNECTION_FAILED));
        }
    };

    if exists {
        if *if_not_exists {
            println!("index '{name}' already exists, no changes (--if-not-exists)");
            return Ok(ExitCode::from(exit::unix::SUCCESS));
        }
        eprintln!(
            "error: index '{name}' already exists (pass --if-not-exists to make this idempotent)"
        );
        return Ok(ExitCode::from(exit::unix::GENERAL_ERROR));
    }

    let body = match read_mapping_file(mapping) {
        Ok(body) => body,
        Err(err) => {
            eprintln!("error: {err:#}");
            return Ok(ExitCode::from(exit::unix::ARGUMENT_ERROR));
        }
    };

    let plan = PlanPreview {
        actions: vec![PlannedAction {
            kind: ActionKind::Create,
            target: name.clone(),
            detail: format!("create index from mapping file {}", mapping.display()),
            estimated_records: None,
        }],
    };

    match guard::authorize(ctx, &plan, confirm_name.as_deref())? {
        GuardDecision::DryRun => Ok(ExitCode::from(exit::unix::SUCCESS)),
        GuardDecision::Declined => Ok(ExitCode::from(exit::unix::CONFIRMATION_DECLINED)),
        GuardDecision::Proceed => match create_index(&os_client, ctx.timeout, name, body).await {
            Ok(()) => {
                println!("index '{name}' created");
                Ok(ExitCode::from(exit::unix::SUCCESS))
            }
            Err(err) => {
                eprintln!("error: {err:#}");
                Ok(ExitCode::from(exit::unix::GENERAL_ERROR))
            }
        },
    }
}

fn read_mapping_file(path: &Path) -> Result<Value> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read mapping file: {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("mapping file is not valid JSON: {}", path.display()))
}

/// `true` if `name` already exists. Shared with [`super::run_reset`].
pub(super) async fn index_exists(
    client: &OpenSearch,
    timeout: Duration,
    name: &str,
) -> Result<bool> {
    let names = [name];
    let indices = client.indices();
    let fut = indices.exists(IndicesExistsParts::Index(&names)).send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("indices.exists timed out")?
        .context("failed to call indices.exists")?;
    match response.status_code().as_u16() {
        200 => Ok(true),
        404 => Ok(false),
        other => bail!("opensearch returned unexpected HTTP {other} for indices.exists"),
    }
}

/// Create `name` with request body `body`. Shared with [`super::apply_reset`].
pub(super) async fn create_index(
    client: &OpenSearch,
    timeout: Duration,
    name: &str,
    body: Value,
) -> Result<()> {
    let indices = client.indices();
    let fut = indices
        .create(IndicesCreateParts::Index(name))
        .body(body)
        .send();
    let response = tokio::time::timeout(timeout, fut)
        .await
        .context("indices.create timed out")?
        .context("failed to call indices.create")?;
    ensure_success(response, "indices.create").await
}

/// Best-effort mapping fetch for `super::apply_reset`'s "preserve the
/// mapping across drop+create" behavior. Returns `{"mappings": {...}}`
/// suitable for [`create_index`]'s body, or `None` if the index has no
/// mapping, or the fetch itself fails -- reset falls back to an empty index
/// rather than aborting on a mapping-preservation hiccup.
pub(super) async fn fetch_mapping_for_recreate(
    client: &OpenSearch,
    timeout: Duration,
    name: &str,
) -> Option<Value> {
    let names = [name];
    let indices = client.indices();
    let fut = indices
        .get_mapping(IndicesGetMappingParts::Index(&names))
        .send();
    let response = tokio::time::timeout(timeout, fut).await.ok()?.ok()?;
    if !response.status_code().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    let mappings = body.get(name)?.get("mappings")?.clone();
    if mappings.as_object().is_some_and(|m| m.is_empty()) {
        return None;
    }
    Some(serde_json::json!({ "mappings": mappings }))
}

/// Shared HTTP-status check for the mutating calls in this module and
/// `super::apply_reset`'s `indices.delete` call. opensearch-rs does not
/// error on non-2xx responses itself (see `os::stats`'s identical pattern),
/// so every mutating call here checks status explicitly instead of trusting
/// `send()`'s `Result` alone.
pub(super) async fn ensure_success(response: Response, op: &str) -> Result<()> {
    let status = response.status_code();
    if status.is_success() {
        return Ok(());
    }
    let body_text = response.text().await.unwrap_or_default();
    bail!("opensearch {op} returned HTTP {status}: {body_text}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_mapping_file_parses_valid_json() {
        let path = std::env::temp_dir().join(format!(
            "dbops-init-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            r#"{"mappings":{"properties":{"f":{"type":"text"}}}}"#,
        )
        .unwrap();
        let body = read_mapping_file(&path).unwrap();
        assert_eq!(body["mappings"]["properties"]["f"]["type"], "text");
        fs::remove_file(path).ok();
    }

    #[test]
    fn read_mapping_file_rejects_invalid_json() {
        let path = std::env::temp_dir().join(format!(
            "dbops-init-test-bad-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "{not valid json").unwrap();
        let err = read_mapping_file(&path).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
        fs::remove_file(path).ok();
    }

    #[test]
    fn read_mapping_file_reports_missing_file() {
        let path = Path::new("/nonexistent/dbops-init-test/mapping.json");
        let err = read_mapping_file(path).unwrap_err();
        assert!(err.to_string().contains("failed to read mapping file"));
    }
}
