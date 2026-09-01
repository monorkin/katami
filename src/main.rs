mod cards;
mod completions;
mod curator;
mod embeddings;
mod flock;
mod fsutil;
mod hook_client;
mod hook_protocol;
mod launch;
mod launches;
mod memory;
mod memory_cli;
mod overlay;
mod paths;
mod pty;
mod reviewer;
mod search;
mod supervisor;
mod transcript;
mod virtual_skills;

use anyhow::Result;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use usage::{Cli, Subcommands};

/// Supervisor for Claude Code: wraps a claude launch and learns from it
#[derive(Cli)]
#[usage(bin = "agent", version, unknown_flags = "error", completion)]
struct Cli {
    /// Command used to launch claude, split on whitespace
    #[usage(long, default = "claude")]
    claude_cmd: String,
    /// Launch directly without the supervisor
    #[usage(long)]
    exec: bool,
    /// Arguments forwarded to claude
    #[usage(double_dash = "required")]
    claude_args: Vec<String>,
    #[usage(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommands)]
enum Command {
    /// Relay a Claude Code hook event to the supervisor
    #[usage(hide = true)]
    Hook { event: String },
    /// Distill a transcript delta into memories (spawned by the supervisor)
    #[usage(hide = true)]
    Review {
        #[usage(long)]
        transcript: PathBuf,
        #[usage(long)]
        config_dir: PathBuf,
    },
    /// Consolidate memories and retire unused skills (spawned by the supervisor)
    #[usage(hide = true)]
    Curate {
        #[usage(long)]
        config_dir: PathBuf,
    },
    /// Inspect and manage the memory store
    Memory {
        #[usage(subcommand)]
        command: MemoryCommand,
    },
    /// Print or install the shell completion script
    ShellCompletion {
        #[usage(subcommand)]
        command: ShellCompletionCommand,
    },
}

#[derive(Subcommands)]
enum MemoryCommand {
    /// Store a memory
    Add {
        title: String,
        body: String,
        /// Entity this belongs to, like project:/path or person:name
        #[usage(long)]
        entity: Option<String>,
        /// Titles of related memories; [[links]] in the body are picked up too
        #[usage(long)]
        link: Vec<String>,
        /// Store it as an entity card instead of an observation
        #[usage(long)]
        card: bool,
    },
    /// Search memories
    Search { query: String },
    /// Show one memory with its links
    Show { id: i64 },
    /// List all memories
    List,
    /// Download the embedding model that powers semantic search
    PullModels,
    /// Consolidate observations into cards and retire unused skills now
    Curate,
}

#[derive(Subcommands)]
enum ShellCompletionCommand {
    /// Write the completion script to stdout
    Print {
        /// The shell to generate for
        #[usage(
            choices("bash", "elvish", "zsh", "fish", "nu", "powershell"),
            choices_strict = false
        )]
        shell: String,
    },
    /// Write the completion script where the shell looks for it
    Install {
        /// The shell to install for
        #[usage(
            choices("bash", "elvish", "zsh", "fish", "nu", "powershell"),
            choices_strict = false
        )]
        shell: String,
    },
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Hook { event }) => hook_client::run(&event),
        Some(Command::Review {
            transcript,
            config_dir,
        }) => reviewer::run(&transcript, &config_dir),
        Some(Command::Curate { config_dir }) => curator::run(&config_dir),
        Some(Command::Memory { command }) => match command {
            MemoryCommand::Add {
                title,
                body,
                entity,
                link,
                card,
            } => memory_cli::add(&title, &body, entity, link, card),
            MemoryCommand::Search { query } => memory_cli::search(&query),
            MemoryCommand::Show { id } => memory_cli::show(id),
            MemoryCommand::List => memory_cli::list(),
            MemoryCommand::PullModels => embeddings::pull(),
            MemoryCommand::Curate => curator::run(&paths::claude_config_home()),
        },
        Some(Command::ShellCompletion { command }) => match command {
            ShellCompletionCommand::Print { shell } => completions::print(&shell),
            ShellCompletionCommand::Install { shell } => completions::install(&shell),
        },
        None => launch::run(&cli.claude_cmd, &cli.claude_args, cli.exec),
    }
}

pub fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs();
    format_epoch_seconds(seconds)
}

fn format_epoch_seconds(seconds: u64) -> String {
    let days_since_epoch = seconds / 86_400;
    let (year, month, day) = civil_date(days_since_epoch);
    let hour = seconds / 3600 % 24;
    let minute = seconds / 60 % 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_date(days_since_epoch: u64) -> (u64, u64, u64) {
    let days = days_since_epoch + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}
