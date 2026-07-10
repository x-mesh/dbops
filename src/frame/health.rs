use clap::Args;

/// Shared threshold flags for every `health` subcommand across domains.
#[derive(Args, Debug)]
pub struct HealthArgs {
    #[arg(long, value_name = "VAL")]
    pub warning: Option<String>,

    #[arg(long, value_name = "VAL")]
    pub critical: Option<String>,
}
