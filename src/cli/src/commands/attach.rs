//! `a3s-box attach` command — attach to a running box.
//!
//! Without `-it`, tails the console log (read-only, original behavior).
//! With `-it`, opens an interactive PTY session to a shell inside the box.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct AttachArgs {
    /// Box name or ID
    pub r#box: String,

    /// Keep STDIN open
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long = "tty")]
    pub tty: bool,
}

pub async fn execute(args: AttachArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    // Interactive PTY mode
    if args.tty {
        return execute_pty_attach(record).await;
    }

    // Original behavior: tail console log
    let console_log = record.console_log.clone();
    if !console_log.exists() {
        return Err(format!(
            "Console log not found for box {} at {}",
            record.name,
            console_log.display()
        )
        .into());
    }

    println!("Attached to box {}. Press Ctrl-C to detach.", record.name);

    let log_handle = tokio::spawn(async move {
        super::tail_file(&console_log).await;
    });

    let _ = tokio::signal::ctrl_c().await;
    println!("\nDetached from box {}.", record.name);

    log_handle.abort();

    Ok(())
}

/// Attach to a running box with an interactive PTY session.
async fn execute_pty_attach(
    record: &crate::state::BoxRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::pty::PtyRequest;
    use a3s_box_runtime::PtyClient;
    use crossterm::terminal;

    let pty_socket_path = record.box_dir.join("sockets").join("pty.sock");
    if !pty_socket_path.exists() {
        return Err(format!(
            "PTY socket not found for box {} (guest may not support interactive mode)",
            record.name,
        )
        .into());
    }

    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    let mut client = PtyClient::connect(&pty_socket_path).await?;

    // Attach opens a shell
    let request = PtyRequest {
        cmd: vec!["/bin/sh".to_string()],
        env: vec![],
        working_dir: None,
        user: None,
        cols,
        rows,
    };
    client.send_request(&request).await?;

    terminal::enable_raw_mode()?;

    let (read_half, write_half) = client.into_split();
    let exit_code = super::exec::run_pty_session(read_half, write_half).await;

    terminal::disable_raw_mode()?;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}
