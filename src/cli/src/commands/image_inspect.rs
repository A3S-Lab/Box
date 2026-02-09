//! `a3s-box image-inspect` command — display detailed image metadata as JSON.

use clap::Args;

#[derive(Args)]
pub struct ImageInspectArgs {
    /// Image reference to inspect
    pub image: String,
}

pub async fn execute(args: ImageInspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;

    let stored = store
        .get(&args.image)
        .await
        .ok_or_else(|| format!("Image not found: {}", args.image))?;

    // Load OCI image to get full config
    let oci = a3s_box_runtime::OciImage::from_path(&stored.path)?;
    let config = oci.config();

    let env_map: serde_json::Map<String, serde_json::Value> = config
        .env
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();

    let output = serde_json::json!({
        "Reference": stored.reference,
        "Digest": stored.digest,
        "Size": stored.size_bytes,
        "PulledAt": stored.pulled_at.to_rfc3339(),
        "Config": {
            "Entrypoint": config.entrypoint,
            "Cmd": config.cmd,
            "Env": env_map,
            "WorkingDir": config.working_dir,
            "User": config.user,
            "ExposedPorts": config.exposed_ports,
            "Labels": config.labels,
        },
        "LayerCount": oci.layer_paths().len(),
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
