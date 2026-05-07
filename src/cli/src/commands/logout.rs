//! `a3s-box logout` command — Remove stored registry credentials.

use clap::Args;

#[derive(Args)]
pub struct LogoutArgs {
    /// Registry server (default: ~/.a3s/config.json registry.default or Docker Hub)
    pub server: Option<String>,
}

pub async fn execute(args: LogoutArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config = a3s_box_core::A3sConfig::load_default()?;
    let server_input = args
        .server
        .unwrap_or_else(|| config.registry.default_login_registry());
    let server = a3s_box_core::normalize_registry_server(&server_input);

    let store = a3s_box_runtime::CredentialStore::default_path()?;
    let removed = store.remove(&server)?;

    if removed {
        println!("Removing login credentials for {}", server);
    } else {
        println!("Not logged in to {}", server);
    }

    Ok(())
}
