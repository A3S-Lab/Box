use super::*;
use a3s_box_runtime::VmLocalExecutionBackend;
use std::sync::atomic::AtomicU64;

// --- build_resource_limits tests (using new struct layout) ---

fn default_run_args() -> RunArgs {
    RunArgs {
        common: common::CommonBoxArgs {
            image: "test".to_string(),
            isolation: None,
            name: None,
            cpus: 2,
            memory: "512m".to_string(),
            volumes: vec![],
            env: vec![],
            publish: vec![],
            dns: vec![],
            entrypoint: None,
            hostname: None,
            user: None,
            workdir: None,
            restart: "no".to_string(),
            labels: vec![],
            tmpfs: vec![],
            virtiofs_cache: None,
            network: None,
            health_cmd: None,
            health_interval: 30,
            health_timeout: 5,
            health_retries: 3,
            health_start_period: 0,
            pids_limit: None,
            cpuset_cpus: None,
            ulimits: vec![],
            cpu_shares: None,
            cpu_quota: None,
            cpu_period: None,
            memory_reservation: None,
            memory_swap: None,
            env_file: vec![],
            add_host: vec![],
            platform: None,
            init: false,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            device: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            no_healthcheck: false,
            oom_kill_disable: false,
            oom_score_adj: None,
            persistent: false,
        },
        detach: false,
        interactive: false,
        no_stdin: false,
        tty: false,
        timeout: None,
        rm: false,
        pool: false,
        pool_socket: DEFAULT_SOCKET.to_string(),
        pool_autostart: false,
        pool_exec: false,
        package_cache: vec![],
        cmd: vec![],
        log_driver: "json-file".to_string(),
        log_opts: vec![],
        tee: false,
        tee_workload_id: None,
        tee_simulate: false,
        sidecar: None,
        sidecar_vsock_port: 4092,
    }
}

fn default_pool_run_args() -> RunArgs {
    let mut args = default_run_args();
    args.pool = true;
    args.rm = true;
    args.cmd = vec!["echo".to_string(), "hello".to_string()];
    args
}

#[path = "tests/lifecycle.rs"]
mod lifecycle;
#[path = "tests/options.rs"]
mod options;
#[path = "request_tests.rs"]
mod request_tests;
