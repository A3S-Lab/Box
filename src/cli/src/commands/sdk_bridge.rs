//! Machine-only bridge for the Python and TypeScript local SDKs.

use std::io::Read;

use clap::Args;

#[derive(Args)]
pub struct SdkBridgeArgs {}

pub async fn execute(_args: SdkBridgeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let response = a3s_box_sdk::bridge::dispatch_json(&input).await;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}
