//! Clap argument models for Compose commands.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use super::super::common;
use super::operations::{
    ComposeCpArgs, ComposeExecArgs, ComposeKillArgs, ComposeLsArgs, ComposePortArgs,
    ComposePullArgs, ComposeRestartArgs, ComposeRmArgs, ComposeStopArgs, ComposeTopArgs,
    ComposeWaitArgs, ProjectServicesArgs,
};

#[derive(Args)]
pub struct ComposeArgs {
    /// Path to compose file (default: compose.acl, then Compose YAML names)
    #[arg(short = 'f', long = "file")]
    pub file: Option<PathBuf>,

    /// Project name (default: directory name)
    #[arg(short = 'p', long = "project-name")]
    pub project_name: Option<String>,

    #[command(subcommand)]
    pub command: ComposeCommand,
}

#[derive(Subcommand)]
pub enum ComposeCommand {
    /// Create and start all services
    Up(ComposeUpArgs),
    /// Stop and remove all services
    Down(ComposeDownArgs),
    /// List services and their status
    Ps(ProjectServicesArgs),
    /// Validate and display the compose configuration
    Config,
    /// View logs from all services
    Logs(ComposeLogsArgs),
    /// Start existing service boxes
    Start(ProjectServicesArgs),
    /// Stop running service boxes without removing them
    Stop(ComposeStopArgs),
    /// Restart service boxes
    Restart(ComposeRestartArgs),
    /// Remove stopped service boxes
    Rm(ComposeRmArgs),
    /// Force-stop service boxes with a signal
    Kill(ComposeKillArgs),
    /// Pause running service boxes
    Pause(ProjectServicesArgs),
    /// Resume paused service boxes
    Unpause(ProjectServicesArgs),
    /// Wait for service boxes to stop
    Wait(ComposeWaitArgs),
    /// Execute a command in a running service box
    Exec(ComposeExecArgs),
    /// Display running processes for service boxes
    Top(ComposeTopArgs),
    /// Print published ports for a service
    Port(ComposePortArgs),
    /// Copy files between a service box and the host
    Cp(ComposeCpArgs),
    /// List images declared by services
    Images(ProjectServicesArgs),
    /// Pull service images
    Pull(ComposePullArgs),
    /// List Compose projects known to A3S Box
    Ls(ComposeLsArgs),
    /// List named volumes declared by the project
    Volumes,
}

#[derive(Args)]
pub struct ComposeUpArgs {
    /// Run in detached mode (background)
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Timeout in seconds to wait for healthy dependencies (default: 120)
    #[arg(long, default_value = "120")]
    pub timeout: u64,

    /// Use the shared-kernel sandbox backend (omit for MicroVM isolation)
    #[arg(long, value_enum)]
    pub isolation: Option<common::IsolationArg>,

    /// Limit convergence to these services and their dependencies
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args)]
pub struct ComposeDownArgs {
    /// Remove named volumes declared in the compose file
    #[arg(short = 'v', long)]
    pub volumes: bool,
}

#[derive(Args)]
pub struct ComposeLogsArgs {
    /// Follow log output
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Number of lines to show from the end of the logs
    #[arg(long, default_value = "100")]
    pub tail: usize,

    /// Limit output to these services
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}
