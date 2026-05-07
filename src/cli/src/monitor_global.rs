//! Global container monitor singleton.
//!
//! Provides a global instance of the container monitor that can be accessed
//! from anywhere in the CLI application.

use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::monitor::ContainerMonitor;

/// Global container monitor instance.
static MONITOR: OnceCell<Arc<ContainerMonitor>> = OnceCell::const_new();

/// Get or initialize the global container monitor.
pub async fn get_monitor() -> Arc<ContainerMonitor> {
    MONITOR
        .get_or_init(|| async {
            let home = a3s_box_core::dirs_home();
            let state_path = home.join("boxes.json");
            let monitor = ContainerMonitor::new(state_path);

            // Start the monitor daemon
            if let Err(e) = monitor.start().await {
                tracing::warn!(error = %e, "Failed to start container monitor");
            }

            Arc::new(monitor)
        })
        .await
        .clone()
}

/// Notify the monitor about a new container.
pub async fn notify_container_started(box_id: String, pid: u32) {
    let monitor = get_monitor().await;

    // Load the container record and add to monitoring
    if let Ok(state) = crate::state::StateFile::load_default() {
        if let Some(record) = state.find_by_id(&box_id) {
            monitor.add_container(record.clone()).await;
            monitor.update_container(&box_id, pid).await;
        }
    }
}

/// Notify the monitor that a container was stopped by the user.
pub async fn notify_container_stopped(box_id: &str) {
    let monitor = get_monitor().await;
    monitor.remove_container(box_id).await;
}

/// Shutdown the global monitor.
pub async fn shutdown_monitor() {
    if let Some(monitor) = MONITOR.get() {
        monitor.stop().await;
    }
}
