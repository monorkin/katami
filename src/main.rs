mod cards;
mod clock;
mod completions;
mod curator;
mod distiller;
mod embeddings;
mod flock;
mod fsutil;
mod hook_client;
mod hook_protocol;
mod launch;
mod launches;
mod log_cli;
mod logs;
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
        #[usage(long)]
        cwd: Option<PathBuf>,
    },
    /// Consolidate memories and archive unused skills (spawned by the supervisor)
    #[usage(hide = true)]
    Curate {
        #[usage(long)]
        config_dir: PathBuf,
    },
    /// Show what the supervisor, reviewer, and curator have been doing
    Log {
        /// Number of recent lines to show
        #[usage(long, default = "50")]
        lines: usize,
        /// Keep printing new activity as it happens
        #[usage(long, short = 'f')]
        follow: bool,
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
    /// Open a memory in $EDITOR
    Edit { id: i64 },
    /// Archive a memory so it stops being injected
    Archive { id: i64 },
    /// Bring an archived memory back
    Unarchive { id: i64 },
    /// List memories with their usage counts
    List {
        /// Include archived memories
        #[usage(long)]
        with_archived: bool,
        /// Show only archived memories
        #[usage(long)]
        archived: bool,
    },
    /// Download the embedding model that powers semantic search
    PullModels,
    /// Consolidate observations into cards and archive unused skills now
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
            cwd,
        }) => reviewer::run(&transcript, &config_dir, cwd.as_deref()),
        Some(Command::Curate { config_dir }) => curator::run(&config_dir),
        Some(Command::Log { lines, follow }) => log_cli::print(lines, follow),
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
            MemoryCommand::Edit { id } => memory_cli::edit(id),
            MemoryCommand::Archive { id } => memory_cli::archive(id),
            MemoryCommand::Unarchive { id } => memory_cli::unarchive(id),
            MemoryCommand::List {
                with_archived,
                archived,
            } => {
                let filter = if archived {
                    memory::ListFilter::ArchivedOnly
                } else if with_archived {
                    memory::ListFilter::All
                } else {
                    memory::ListFilter::Active
                };
                memory_cli::list(filter)
            }
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

