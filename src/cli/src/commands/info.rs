//! `a3s-box info` command.

use clap::Args;

use crate::state::StateFile;

#[derive(Args)]
pub struct InfoArgs;

pub async fn execute(_args: InfoArgs) -> Result<(), Box<dyn std::error::Error>> {
    println!("a3s-box version {}", a3s_box_core::VERSION);

    // Virtualization support
    match a3s_box_runtime::check_virtualization_support() {
        Ok(support) => {
            println!("Virtualization: {} ({})", support.backend, support.details);
        }
        Err(e) => {
            println!("Virtualization: not available ({e})");
        }
    }

    // Home directory
    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| std::path::PathBuf::from(".a3s"));
    println!("Home directory: {}", home.display());

    // Box count
    match StateFile::load_default() {
        Ok(state) => {
            let all = state.list(true);
            let running = state.list(false);
            println!("Boxes: {} total, {} running", all.len(), running.len());
        }
        Err(_) => {
            println!("Boxes: 0 total, 0 running");
        }
    }

    // Image cache stats
    let images_dir = home.join("images");
    if images_dir.exists() {
        let store = a3s_box_runtime::ImageStore::new(&images_dir, 10 * 1024 * 1024 * 1024);
        match store {
            Ok(store) => {
                let images = store.list().await;
                let total_size: u64 = images.iter().map(|i| i.size_bytes).sum();
                println!(
                    "Images: {} cached ({})",
                    images.len(),
                    crate::output::format_bytes(total_size)
                );
            }
            Err(_) => {
                println!("Images: 0 cached");
            }
        }
    } else {
        println!("Images: 0 cached");
    }

    Ok(())
}
