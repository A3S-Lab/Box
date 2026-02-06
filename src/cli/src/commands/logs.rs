//! `a3s-box logs` command — View box console logs.

use std::io::{BufRead, BufReader, Seek, SeekFrom};

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct LogsArgs {
    /// Box name or ID
    pub r#box: String,

    /// Follow log output
    #[arg(short, long)]
    pub follow: bool,

    /// Number of lines to show from the end
    #[arg(long)]
    pub tail: Option<usize>,
}

pub async fn execute(args: LogsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    let log_path = &record.console_log;
    if !log_path.exists() {
        return Err(format!("No logs found for box {}", record.name).into());
    }

    if let Some(tail_n) = args.tail {
        // Print last N lines
        let file = std::fs::File::open(log_path)?;
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;
        let start = lines.len().saturating_sub(tail_n);
        for line in &lines[start..] {
            println!("{line}");
        }
    } else if !args.follow {
        // Print entire file
        let content = std::fs::read_to_string(log_path)?;
        print!("{content}");
    }

    if args.follow {
        // Follow mode: seek to end and poll for new content
        let file = std::fs::File::open(log_path)?;
        let mut reader = BufReader::new(file);

        if args.tail.is_none() {
            // Start from the end
            reader.seek(SeekFrom::End(0))?;
        }

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                }
                Ok(_) => {
                    print!("{line}");
                }
                Err(e) => {
                    return Err(format!("Error reading log: {e}").into());
                }
            }
        }
    }

    Ok(())
}
