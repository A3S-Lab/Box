//! `a3s-box port` command — List port mappings for a box.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct PortArgs {
    /// Box name or ID
    pub r#box: String,
}

pub async fn execute(args: PortArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.port_map.is_empty() {
        // No port mappings — silent (matches Docker behavior)
        return Ok(());
    }

    for mapping in &record.port_map {
        // Format: "host_port:guest_port" → "guest_port/tcp -> 0.0.0.0:host_port"
        if let Some((host_port, guest_port)) = mapping.split_once(':') {
            println!("{}/tcp -> 0.0.0.0:{}", guest_port, host_port);
        } else {
            // Single port: same on both sides
            println!("{}/tcp -> 0.0.0.0:{}", mapping, mapping);
        }
    }

    Ok(())
}
