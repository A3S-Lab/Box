//! `a3s-box login` command — Store registry credentials.

use clap::Args;

#[derive(Args)]
pub struct LoginArgs {
    /// Registry server (default: ~/.a3s/config.json registry.default or Docker Hub)
    pub server: Option<String>,

    /// Username
    #[arg(short, long)]
    pub username: Option<String>,

    /// Password
    #[arg(short, long, conflicts_with = "password_stdin")]
    pub password: Option<String>,

    /// Read password from stdin
    #[arg(long)]
    pub password_stdin: bool,

    /// Store credentials without contacting the registry
    #[arg(long)]
    pub skip_verify: bool,
}

pub async fn execute(args: LoginArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config = a3s_box_core::A3sConfig::load_default()?;
    let server_input = args
        .server
        .unwrap_or_else(|| config.registry.default_login_registry());
    let server = a3s_box_core::normalize_registry_server(&server_input);

    let username = match args.username {
        Some(u) => u,
        None => {
            eprint!("Username: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };

    let password = if args.password_stdin {
        let mut input = String::new();
        let mut stdin = std::io::stdin();
        std::io::Read::read_to_string(&mut stdin, &mut input)?;
        input.trim().to_string()
    } else {
        match args.password {
            Some(p) => p,
            None => {
                eprint!("Password: ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                input.trim().to_string()
            }
        }
    };

    if username.is_empty() || password.is_empty() {
        return Err("Username and password are required".into());
    }

    if config.registry.login_verify && !args.skip_verify {
        let insecure = a3s_box_core::registry_uses_http(&server_input)
            || config.registry.is_insecure_registry(&server);
        let verifier =
            a3s_box_runtime::RegistryLoginVerifier::new(a3s_box_runtime::RegistryLoginOptions {
                insecure,
            })?;
        verifier.verify(&server, &username, &password).await?;
    }

    let store = a3s_box_runtime::CredentialStore::default_path()?;
    store.store(&server, &username, &password)?;

    println!("Login Succeeded");
    Ok(())
}
