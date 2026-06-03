//! `a3s-box inspect` command — Detailed box information as JSON.

use clap::Args;
use serde::Serialize;

use crate::resolve::{self, ResolveError};
use crate::state::{BoxRecord, StateFile};
use crate::status;

use super::image_inspect;

#[derive(Args)]
pub struct InspectArgs {
    /// Container or image name/ID
    pub r#box: String,
}

pub async fn execute(args: InspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;

    // `docker inspect` is polymorphic: try a container first, then fall back to
    // an image so `inspect <image>` works the same as `inspect <container>`.
    match resolve::resolve(&state, &args.r#box) {
        Ok(record) => {
            println!("{}", inspect_json(record)?);
            Ok(())
        }
        Err(ResolveError::NotFound(_)) => {
            match image_inspect::try_image_inspect_json(&args.r#box).await? {
                Some(json) => {
                    println!("{json}");
                    Ok(())
                }
                None => Err(format!("No such container or image: {}", args.r#box).into()),
            }
        }
        Err(other) => Err(other.into()),
    }
}

#[derive(Serialize)]
struct InspectView<'a> {
    #[serde(flatten)]
    record: &'a BoxRecord,
    status_detail: status::StatusDetails,
}

fn inspect_json(record: &BoxRecord) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&InspectView {
        record,
        status_detail: status::status_details(record),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_inspect_json_includes_status_detail() {
        let mut record = make_record("id", "box", "dead", None);
        record.exit_code = Some(137);

        let json = inspect_json(&record).unwrap();

        assert!(json.contains("\"status\": \"dead\""));
        assert!(json.contains("\"status_detail\""));
        assert!(json.contains("\"summary\": \"dead (Exit 137)\""));
        assert!(json.contains("a3s-box restart box"));
    }
}
