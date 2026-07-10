use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{mongo, net, os, pg, redis, sys};

#[derive(Parser, Debug)]
#[command(name = "dbops", version, about = "SRE database operations toolkit", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help = "Named connection profile to use"
    )]
    pub profile: Option<String>,

    #[arg(long, global = true, value_name = "PATH", help = "Path to config file")]
    pub config: Option<PathBuf>,

    #[arg(long, global = true, help = "Emit machine-readable JSON output")]
    pub json: bool,

    #[arg(
        long,
        global = true,
        value_name = "DUR",
        help = "Operation timeout (e.g. 5s, 500ms)"
    )]
    pub timeout: Option<String>,

    #[arg(
        long = "dry-run",
        global = true,
        help = "Print intended actions without executing them"
    )]
    pub dry_run: bool,

    #[arg(long, global = true, help = "Assume yes for confirmation prompts")]
    pub yes: bool,

    #[arg(long, global = true, help = "Skip TLS certificate verification")]
    pub insecure: bool,

    #[arg(
        short = 'v',
        long = "verbose",
        global = true,
        action = clap::ArgAction::Count,
        help = "Increase verbosity (-v, -vv, -vvv)"
    )]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// OpenSearch/Elasticsearch-compatible cluster operations
    Os(os::OsArgs),
    /// MongoDB operations
    Mongo(mongo::MongoArgs),
    /// PostgreSQL operations
    Pg(pg::PgArgs),
    /// Redis operations
    Redis(redis::RedisArgs),
    /// HTTP endpoint checks
    Http(net::HttpArgs),
    /// TCP endpoint checks
    Tcp(net::TcpArgs),
    /// Local host system checks
    Sys(sys::SysArgs),
    /// Generate a shell completion script (source or install it yourself,
    /// e.g. `dbops completion bash > /etc/bash_completion.d/dbops`)
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}
