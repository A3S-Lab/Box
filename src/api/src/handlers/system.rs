//! System API handlers.

use axum::{Json, extract::Query, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiResult;

/// GET /_ping - Ping the server.
pub async fn ping() -> impl IntoResponse {
    "OK"
}

/// GET /version - Get version information.
pub async fn version() -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "Version": env!("CARGO_PKG_VERSION"),
        "ApiVersion": "1.43",
        "MinAPIVersion": "1.12",
        "GitCommit": env!("VERGEN_GIT_SHA").get(..7).unwrap_or("unknown"),
        "GoVersion": "N/A",
        "Os": std::env::consts::OS,
        "Arch": std::env::consts::ARCH,
        "KernelVersion": "N/A",
        "BuildTime": env!("VERGEN_BUILD_TIMESTAMP"),
        "Experimental": false,
    })))
}

/// GET /info - Get system information.
pub async fn info() -> ApiResult<Json<serde_json::Value>> {
    // Load state to get container count
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| crate::error::ApiError::Internal(e.to_string()))?;

    let containers = state.list(true);
    let running = containers.iter().filter(|c| c.status == "running").count();
    let stopped = containers.iter().filter(|c| c.status != "running").count();

    // Get image count from store
    let image_count = {
        let store_path = a3s_box_core::dirs_home().join("images");
        if let Ok(store) = a3s_box_runtime::oci::ImageStore::new(store_path) {
            store.list().map(|imgs| imgs.len()).unwrap_or(0)
        } else {
            0
        }
    };

    // Get system memory
    let mem_total = {
        use sysinfo::System;
        let mut sys = System::new();
        sys.refresh_memory();
        sys.total_memory()
    };

    Ok(Json(json!({
        "ID": uuid::Uuid::new_v4().to_string(),
        "Containers": containers.len(),
        "ContainersRunning": running,
        "ContainersPaused": 0,
        "ContainersStopped": stopped,
        "Images": image_count,
        "Driver": "a3s-box",
        "DriverStatus": [],
        "SystemStatus": null,
        "Plugins": {
            "Volume": ["local"],
            "Network": ["bridge", "tsi"],
            "Authorization": null,
            "Log": ["json-file"],
        },
        "MemoryLimit": true,
        "SwapLimit": false,
        "KernelMemory": false,
        "CpuCfsPeriod": true,
        "CpuCfsQuota": true,
        "CPUShares": true,
        "CPUSet": true,
        "PidsLimit": true,
        "IPv4Forwarding": true,
        "BridgeNfIptables": true,
        "BridgeNfIp6tables": true,
        "Debug": false,
        "NFd": 0,
        "OomKillDisable": true,
        "NGoroutines": 0,
        "SystemTime": chrono::Utc::now().to_rfc3339(),
        "LoggingDriver": "json-file",
        "CgroupDriver": "cgroupfs",
        "NEventsListener": 0,
        "KernelVersion": std::env::consts::OS,
        "OperatingSystem": "A3S Box",
        "OSType": std::env::consts::OS,
        "Architecture": std::env::consts::ARCH,
        "IndexServerAddress": "https://index.docker.io/v1/",
        "RegistryConfig": {},
        "NCPU": num_cpus::get(),
        "MemTotal": mem_total,
        "GenericResources": null,
        "DockerRootDir": a3s_box_core::dirs_home().display().to_string(),
        "HttpProxy": "",
        "HttpsProxy": "",
        "NoProxy": "",
        "Name": hostname::get().unwrap_or_default().to_string_lossy().to_string(),
        "Labels": [],
        "ExperimentalBuild": false,
        "ServerVersion": env!("CARGO_PKG_VERSION"),
        "Runtimes": {
            "a3s-box": {
                "path": "a3s-box"
            }
        },
        "DefaultRuntime": "a3s-box",
        "Swarm": {
            "NodeID": "",
            "NodeAddr": "",
            "LocalNodeState": "inactive",
            "ControlAvailable": false,
            "Error": "",
            "RemoteManagers": null
        },
        "LiveRestoreEnabled": false,
        "Isolation": "",
        "InitBinary": "",
        "ContainerdCommit": {
            "ID": "",
            "Expected": ""
        },
        "RuncCommit": {
            "ID": "",
            "Expected": ""
        },
        "InitCommit": {
            "ID": "",
            "Expected": ""
        },
        "SecurityOptions": [],
        "Warnings": null
    })))
}

/// Query parameters for events.
#[derive(Debug, Deserialize, Default)]
pub struct EventsQuery {
    /// Show events since timestamp
    since: Option<String>,

    /// Show events until timestamp
    until: Option<String>,

    /// Filter events
    filters: Option<String>,
}

/// GET /events - Stream system events.
pub async fn events(Query(_query): Query<EventsQuery>) -> ApiResult<Json<serde_json::Value>> {
    // Return empty events list for now
    // TODO: Implement real-time event streaming using SSE or chunked transfer
    Ok(Json(json!({
        "Type": "container",
        "Action": "start",
        "Actor": {
            "ID": "",
            "Attributes": {}
        },
        "time": chrono::Utc::now().timestamp(),
        "timeNano": chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    })))
}
