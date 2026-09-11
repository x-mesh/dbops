//! `dbops mongo replset`: per-member replica set status as a stat table.

use anyhow::{bail, Context, Result};
use mongodb::bson::doc;

use crate::frame::result::StatReport;
use crate::frame::{Ctx, ExitCode};
use crate::mongo::replset_status::MemberInfo;
use crate::mongo::{client, replset_status};

pub async fn run(ctx: &Ctx) -> Result<ExitCode> {
    let members = tokio::time::timeout(ctx.timeout, fetch_members(ctx))
        .await
        .context("mongodb replset query timed out")??;

    let report = to_stat_report(&members);
    println!("{}", crate::frame::output::render_stat(&report, ctx.json));
    Ok(ExitCode::from(crate::frame::exit::unix::SUCCESS))
}

async fn fetch_members(ctx: &Ctx) -> Result<Vec<MemberInfo>> {
    let mongo_client = client::connect(&ctx.profile.mongodb, ctx.timeout, ctx.insecure).await?;
    let status_doc = match mongo_client
        .database("admin")
        .run_command(doc! { "replSetGetStatus": 1 })
        .await
    {
        Ok(status_doc) => status_doc,
        Err(err) => bail!("replSetGetStatus failed (is this node part of a replica set?): {err:#}"),
    };
    replset_status::parse_members(&status_doc)
}

fn to_stat_report(members: &[MemberInfo]) -> StatReport {
    let columns = ["name", "state", "health", "lag_seconds", "last_heartbeat"]
        .into_iter()
        .map(String::from)
        .collect();

    let rows = members
        .iter()
        .map(|m| {
            vec![
                m.name.clone(),
                m.state.clone(),
                if m.health >= 1.0 {
                    "healthy".to_string()
                } else {
                    "unhealthy".to_string()
                },
                m.lag_secs
                    .map_or_else(|| "-".to_string(), |secs| secs.to_string()),
                m.last_heartbeat.map_or_else(
                    || "-".to_string(),
                    |dt| {
                        dt.try_to_rfc3339_string()
                            .unwrap_or_else(|_| "-".to_string())
                    },
                ),
            ]
        })
        .collect();

    StatReport { columns, rows }
}
