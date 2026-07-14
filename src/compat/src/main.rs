use std::path::PathBuf;

use a3s_box_compat::{generate_fixture, verify_fixture, FixturePaths};
use anyhow::{bail, Context, Result};

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let action = arguments.next().unwrap_or_else(|| "verify".to_string());
    let mut root = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => {
                root = Some(PathBuf::from(
                    arguments.next().context("--root requires a path")?,
                ));
            }
            _ => bail!("unknown argument {argument}"),
        }
    }
    let paths = root
        .map(FixturePaths::new)
        .unwrap_or_else(FixturePaths::repository_default);
    match action.as_str() {
        "generate" => generate_fixture(&paths),
        "verify" => verify_fixture(&paths),
        _ => bail!("unknown action {action}; expected generate or verify"),
    }
}
