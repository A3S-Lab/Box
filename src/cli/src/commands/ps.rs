//! `a3s-box ps` command — List boxes.

use clap::Args;

use crate::output;
use crate::state::StateFile;

#[derive(Args)]
pub struct PsArgs {
    /// Show all boxes (including stopped)
    #[arg(short, long)]
    pub all: bool,
}

pub async fn execute(args: PsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let boxes = state.list(args.all);

    let mut table = output::new_table(&["BOX ID", "IMAGE", "STATUS", "CREATED", "NAMES"]);

    for record in boxes {
        table.add_row(&[
            &record.short_id,
            &record.image,
            &record.status,
            &output::format_ago(&record.created_at),
            &record.name,
        ]);
    }

    println!("{table}");
    Ok(())
}
