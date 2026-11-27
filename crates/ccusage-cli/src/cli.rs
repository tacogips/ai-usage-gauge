use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "ccusage-gauge",
    version = ccusage_core::version::APP_VERSION,
    about = "Native Rust usage & subscription monitor for Claude Code, Codex, and Antigravity"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Validate configuration and state without requiring a local ccusage binary
    ConfigCheck,
    /// Compute and print a local usage snapshot
    UsageSnapshot {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Run the loopback dashboard HTTP server
    Serve {
        /// Port to bind on 127.0.0.1 (1..=65535)
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=65535))]
        port: Option<u16>,
        /// Explicit static web assets directory
        #[arg(long)]
        assets: Option<PathBuf>,
    },
    /// Launch the Tauri desktop dashboard window (--dashboard-only)
    Dashboard,
    /// Interact with a running loopback dashboard API server
    Client {
        /// Target loopback API port (overrides config dashboardPort)
        #[arg(long, global = true, value_parser = clap::value_parser!(u16).range(1..=65535))]
        port: Option<u16>,
        /// Output raw JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        subcommand: ClientCommands,
    },
}

#[derive(Debug, Subcommand)]
pub enum ClientCommands {
    /// Manage machines registered on the dashboard server
    Machines {
        #[command(subcommand)]
        action: MachineActions,
    },
    /// Query or control dashboard endpoints
    Dashboard {
        #[command(subcommand)]
        action: DashboardActions,
    },
}

#[derive(Debug, Subcommand)]
pub enum MachineActions {
    List,
    Show { id: String },
    Add(AddMachineArgs),
    Update(UpdateMachineArgs),
    Enable { id: String },
    Disable { id: String },
    Remove { id: String },
    TestConnection { id: String },
    Refresh { id: String },
}

#[derive(Debug, Args)]
pub struct AddMachineArgs {
    #[arg(long)]
    pub id: String,
    #[arg(long)]
    pub display_name: String,
    #[arg(long)]
    pub host: String,
    #[arg(long, default_value_t = 22)]
    pub ssh_port: u16,
    #[arg(long)]
    pub user: String,
    #[arg(long)]
    pub identity_file: Option<String>,
    #[arg(long, default_value = "ccusage")]
    pub remote_ccusage_path: String,
    #[arg(long)]
    pub codex_session_dir: Vec<String>,
    #[arg(long)]
    pub claude_config_dir: Vec<String>,
    #[arg(long, conflicts_with = "exclude_default_codex_dir")]
    pub include_default_codex_dir: bool,
    #[arg(long, conflicts_with = "include_default_codex_dir")]
    pub exclude_default_codex_dir: bool,
    #[arg(long, conflicts_with = "exclude_default_claude_dir")]
    pub include_default_claude_dir: bool,
    #[arg(long, conflicts_with = "include_default_claude_dir")]
    pub exclude_default_claude_dir: bool,
}

#[derive(Debug, Args)]
pub struct UpdateMachineArgs {
    pub id: String,
    #[arg(long)]
    pub display_name: Option<String>,
    #[arg(long)]
    pub codex_session_dir: Vec<String>,
    #[arg(long)]
    pub claude_config_dir: Vec<String>,
    #[arg(long)]
    pub clear_codex_session_dirs: bool,
    #[arg(long)]
    pub clear_claude_config_dirs: bool,
    #[arg(long, conflicts_with = "exclude_default_codex_dir")]
    pub include_default_codex_dir: bool,
    #[arg(long, conflicts_with = "include_default_codex_dir")]
    pub exclude_default_codex_dir: bool,
    #[arg(long, conflicts_with = "exclude_default_claude_dir")]
    pub include_default_claude_dir: bool,
    #[arg(long, conflicts_with = "include_default_claude_dir")]
    pub exclude_default_claude_dir: bool,
}

#[derive(Debug, Subcommand)]
pub enum DashboardActions {
    Health,
    Budget {
        #[arg(long)]
        machine: Vec<String>,
        #[arg(long)]
        directory: Vec<String>,
    },
    Recent {
        #[arg(long, default_value_t = 12)]
        hours: i64,
        #[arg(long)]
        machine: Vec<String>,
    },
    Day {
        #[arg(long)]
        date: Option<String>,
        #[arg(long)]
        machine: Vec<String>,
    },
    Period {
        #[arg(long, default_value = "today")]
        range: String,
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
        #[arg(long)]
        machine: Vec<String>,
    },
    Metrics {
        #[arg(long, default_value = "today")]
        range: String,
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
        #[arg(long)]
        machine: Vec<String>,
        #[arg(long)]
        directory: Vec<String>,
    },
    CostSeries {
        #[arg(long, default_value = "today")]
        range: String,
        #[arg(long, default_value = "daily")]
        granularity: String,
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
        #[arg(long)]
        machine: Vec<String>,
        #[arg(long)]
        directory: Vec<String>,
        #[arg(long)]
        directory_breakdown: bool,
        #[arg(long)]
        effort_breakdown: bool,
    },
    MachineStatus,
    LoadStatus,
    Refresh,
    CacheClear {
        #[arg(long)]
        machine: Vec<String>,
    },
}
