//! `a3s-box inspect` command — Detailed box information as JSON.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct InspectArgs {
    /// Box name or ID
    pub r#box: String,
}

pub async fn execute(args: InspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    let json = serde_json::to_string_pretty(record)?;
    println!("{json}");

    Ok(())
}
