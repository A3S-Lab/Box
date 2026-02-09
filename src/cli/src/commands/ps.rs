//! `a3s-box ps` command — List boxes.

use clap::Args;

use crate::output;
use crate::state::{BoxRecord, StateFile};

#[derive(Args)]
pub struct PsArgs {
    /// Show all boxes (including stopped)
    #[arg(short, long)]
    pub all: bool,

    /// Only display box IDs
    #[arg(short, long)]
    pub quiet: bool,

    /// Format output using placeholders: {{.ID}}, {{.Image}}, {{.Status}},
    /// {{.Created}}, {{.Names}}, {{.Ports}}, {{.Command}}
    #[arg(long)]
    pub format: Option<String>,

    /// Filter boxes (e.g., status=running, name=dev, ancestor=alpine)
    #[arg(short, long = "filter")]
    pub filters: Vec<String>,
}

pub async fn execute(args: PsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let boxes = state.list(args.all);

    // Apply filters
    let boxes: Vec<&&BoxRecord> = boxes
        .iter()
        .filter(|r| matches_filters(r, &args.filters))
        .collect();

    // --quiet: print only IDs
    if args.quiet {
        for record in &boxes {
            println!("{}", record.short_id);
        }
        return Ok(());
    }

    // --format: custom template output
    if let Some(ref fmt) = args.format {
        for record in &boxes {
            println!("{}", apply_format(record, fmt));
        }
        return Ok(());
    }

    // Default: table output
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

/// Check if a box record matches all the given filters.
///
/// Supported filters:
/// - `status=<value>` — match box status (running, stopped, created, dead)
/// - `name=<value>` — match box name (substring)
/// - `ancestor=<value>` — match image reference (substring)
/// - `id=<value>` — match box ID prefix
fn matches_filters(record: &BoxRecord, filters: &[String]) -> bool {
    for filter in filters {
        let (key, value) = match filter.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };

        let matched = match key {
            "status" => record.status == value,
            "name" => record.name.contains(value),
            "ancestor" => record.image.contains(value),
            "id" => record.id.starts_with(value) || record.short_id.starts_with(value),
            _ => true, // Ignore unknown filters
        };

        if !matched {
            return false;
        }
    }
    true
}

/// Apply a format template, replacing `{{.Field}}` placeholders.
fn apply_format(record: &BoxRecord, fmt: &str) -> String {
    fmt.replace("{{.ID}}", &record.short_id)
        .replace("{{.Image}}", &record.image)
        .replace("{{.Status}}", &record.status)
        .replace("{{.Created}}", &output::format_ago(&record.created_at))
        .replace("{{.Names}}", &record.name)
        .replace("{{.Command}}", &record.cmd.join(" "))
        .replace("{{.Ports}}", "")
}
