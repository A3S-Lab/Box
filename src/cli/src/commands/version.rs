//! `a3s-box version` command.

use clap::Args;

#[derive(Args)]
pub struct VersionArgs;

pub async fn execute(_args: VersionArgs) -> Result<(), Box<dyn std::error::Error>> {
    println!("a3s-box version {}", a3s_box_core::VERSION);
    Ok(())
}
