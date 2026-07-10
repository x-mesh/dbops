//! A minimal GitHub Releases client: enough to find a release and pull two
//! assets out of it.
//!
//! Assets are always fetched through the *assets API* URL
//! (`/repos/{repo}/releases/assets/{id}` with `Accept:
//! application/octet-stream`) rather than through each asset's
//! `browser_download_url`. That is the only form that works for a private
//! repository, and it works unauthenticated for a public one too — so there
//! is exactly one download path to reason about.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::StatusCode;
use serde::Deserialize;

/// `owner/name` releases are published to. Overridable via
/// `DBOPS_UPDATE_REPO` so a fork — or a staging repo used to rehearse a
/// release — can be exercised without a rebuild.
const DEFAULT_REPO: &str = "x-mesh/dbops";
const API_BASE: &str = "https://api.github.com";
const REPO_ENV_VAR: &str = "DBOPS_UPDATE_REPO";

/// Checked in order; the first non-empty one wins. `GITHUB_TOKEN` and
/// `GH_TOKEN` are what CI and the `gh` CLI already export, so an operator
/// who can read the repo usually has one without doing anything.
const TOKEN_ENV_VARS: [&str; 3] = ["DBOPS_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"];

/// Applies to both clients. Distinct from the per-request timeout: a slow
/// link should be allowed to finish a multi-megabyte download, but an
/// unreachable host should still fail fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

const API_VERSION_HEADER: &str = "X-GitHub-Api-Version";
const API_VERSION: &str = "2022-11-28";

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    /// The assets-API URL, *not* `browser_download_url` — see module docs.
    url: String,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// Every asset name, for the "release X has no asset Y" error message —
    /// a missing artifact is almost always a half-finished release, and
    /// seeing what *did* upload is what tells you that.
    pub fn asset_names(&self) -> Vec<&str> {
        self.assets
            .iter()
            .map(|asset| asset.name.as_str())
            .collect()
    }
}

pub struct Client {
    /// Short-timeout client for the JSON metadata call.
    api: reqwest::Client,
    /// Connect timeout only, no overall deadline: a 6 MB binary over a slow
    /// link must not trip a limit meant for a database health probe.
    bulk: reqwest::Client,
    repo: String,
    token: Option<String>,
}

impl Client {
    /// `api_timeout` bounds the metadata request only.
    ///
    /// Note what is *not* wired up here: the global `--insecure` flag. It
    /// exists to reach a database behind a self-signed certificate; letting
    /// it also disable verification while downloading an executable that
    /// then replaces this process would turn a convenience into a way to
    /// get a hostile binary installed.
    pub fn new(api_timeout: Duration) -> Result<Self> {
        let build = |timeout: Option<Duration>| -> Result<reqwest::Client> {
            let mut builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
            if let Some(timeout) = timeout {
                builder = builder.timeout(timeout);
            }
            builder.build().context("build http client")
        };

        Ok(Self {
            api: build(Some(api_timeout))?,
            bulk: build(None)?,
            repo: std::env::var(REPO_ENV_VAR).unwrap_or_else(|_| DEFAULT_REPO.to_string()),
            token: pick_token(|key| std::env::var(key).ok()),
        })
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// The release for `tag`, or the newest published release when `None`.
    pub async fn release(&self, tag: Option<&str>) -> Result<Release> {
        let url = match tag {
            Some(tag) => format!("{API_BASE}/repos/{}/releases/tags/{tag}", self.repo),
            None => format!("{API_BASE}/repos/{}/releases/latest", self.repo),
        };
        // reqwest is built without its `json` feature (see Cargo.toml), so
        // the body is decoded and parsed in two explicit steps.
        let body = self
            .get(&self.api, &url, "application/vnd.github+json")
            .await?
            .text()
            .await
            .context("read release metadata")?;
        serde_json::from_str(&body).context("parse release metadata as a GitHub release object")
    }

    pub async fn download(&self, asset: &Asset) -> Result<Vec<u8>> {
        let bytes = self
            .get(&self.bulk, &asset.url, "application/octet-stream")
            .await?
            .bytes()
            .await
            .with_context(|| format!("download {}", asset.name))?;
        Ok(bytes.to_vec())
    }

    async fn get(
        &self,
        http: &reqwest::Client,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::Response> {
        let mut request = http
            .get(url)
            .header(ACCEPT, accept)
            .header(USER_AGENT, concat!("dbops/", env!("CARGO_PKG_VERSION")))
            .header(API_VERSION_HEADER, API_VERSION);
        if let Some(token) = &self.token {
            // reqwest strips this on a cross-origin redirect, so it is not
            // forwarded to the CDN host the assets API hands us off to.
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }

        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        bail!("{}", self.explain(status, url))
    }

    /// Turn the HTTP failures that actually happen here into the sentence
    /// that resolves each one.
    ///
    /// Unlike `install.sh`, this never shells out to `gh` for a token — a
    /// static binary on a server has no `gh` — so where a token would help,
    /// the message names the command that mints one.
    fn explain(&self, status: StatusCode, url: &str) -> String {
        let repo = &self.repo;
        let get_a_token = "export GITHUB_TOKEN (on a machine with the gh CLI: \
                           `export GITHUB_TOKEN=$(gh auth token)`)";
        let hint = match status {
            StatusCode::NOT_FOUND if self.token.is_none() => {
                format!("no such release — if {repo} is private, {get_a_token}")
            }
            StatusCode::NOT_FOUND => {
                format!("no such release — check the tag exists and the token can read {repo}")
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if self.token.is_none() => {
                format!(
                    "denied, or rate-limited — unauthenticated GitHub API calls are capped at \
                     60/hour; {get_a_token}"
                )
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                format!("denied — the token cannot read {repo} (it needs `contents: read`)")
            }
            _ => "unexpected response from the GitHub API".to_string(),
        };
        format!("GitHub API returned {status} for {url}: {hint}")
    }
}

/// First non-blank value among [`TOKEN_ENV_VARS`].
///
/// `lookup` is injected rather than reading `std::env` directly so the
/// precedence order is testable without mutating global process state — the
/// same reason `frame::config::resolve` takes an env snapshot.
fn pick_token<F: Fn(&str) -> Option<String>>(lookup: F) -> Option<String> {
    TOKEN_ENV_VARS
        .iter()
        .filter_map(|key| lookup(key))
        .find(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn dbops_token_wins_over_the_generic_ones() {
        let token = pick_token(env(&[
            ("GITHUB_TOKEN", "generic"),
            ("DBOPS_GITHUB_TOKEN", "specific"),
            ("GH_TOKEN", "gh"),
        ]));
        assert_eq!(token.as_deref(), Some("specific"));
    }

    #[test]
    fn gh_token_is_the_last_fallback() {
        assert_eq!(
            pick_token(env(&[("GH_TOKEN", "gh")])).as_deref(),
            Some("gh")
        );
        assert_eq!(pick_token(env(&[])), None);
    }

    /// An exported-but-empty `GITHUB_TOKEN` is common in CI; it must not
    /// shadow a real token further down the list, nor produce an
    /// `Authorization: Bearer ` header.
    #[test]
    fn blank_tokens_are_skipped_not_used() {
        assert_eq!(pick_token(env(&[("GITHUB_TOKEN", "   ")])), None);
        assert_eq!(
            pick_token(env(&[("GITHUB_TOKEN", ""), ("GH_TOKEN", "real")])).as_deref(),
            Some("real")
        );
    }

    /// The two fields this code depends on, taken from a real
    /// `releases/latest` payload (trimmed): the asset's `url` is the
    /// assets-API URL, not `browser_download_url`.
    #[test]
    fn release_json_deserializes_and_finds_assets_by_name() {
        let body = r#"{
          "tag_name": "v0.2.0",
          "name": "v0.2.0",
          "assets": [
            {
              "url": "https://api.github.com/repos/x-mesh/dbops/releases/assets/1",
              "id": 1,
              "name": "dbops-0.2.0-aarch64-apple-darwin",
              "browser_download_url": "https://github.com/x-mesh/dbops/releases/download/v0.2.0/dbops-0.2.0-aarch64-apple-darwin"
            },
            {
              "url": "https://api.github.com/repos/x-mesh/dbops/releases/assets/2",
              "id": 2,
              "name": "SHA256SUMS",
              "browser_download_url": "https://github.com/x-mesh/dbops/releases/download/v0.2.0/SHA256SUMS"
            }
          ]
        }"#;

        let release: Release = serde_json::from_str(body).unwrap();
        assert_eq!(release.tag_name, "v0.2.0");
        assert_eq!(
            release.asset("SHA256SUMS").unwrap().url,
            "https://api.github.com/repos/x-mesh/dbops/releases/assets/2"
        );
        assert!(release
            .asset("dbops-0.2.0-x86_64-unknown-linux-musl")
            .is_none());
        assert_eq!(release.asset_names().len(), 2);
    }

    #[test]
    fn a_release_with_no_assets_parses_as_empty_rather_than_failing() {
        let release: Release = serde_json::from_str(r#"{"tag_name": "v0.1.0"}"#).unwrap();
        assert!(release.asset_names().is_empty());
    }
}
