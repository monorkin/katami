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
mod relays;
mod reviewer;
mod search;
mod supervisor;
mod transcript;
mod transcript_codex;
mod transcript_opencode;
mod transcript_pi;
mod virtual_skills;

use anyhow::Result;
use std::path::{Path, PathBuf};
use usage::{Cli, Subcommands};

/// Supervisor for coding agents: wraps a launch and learns from the session
#[derive(Cli)]
#[usage(bin = "katami", version, unknown_flags = "error", completion)]
struct Cli {
    /// Command used to launch the coding tool, split on whitespace
    #[usage(long, default = "claude")]
    cmd: String,
    /// Deprecated alias for --cmd
    #[usage(long, hide = true)]
    claude_cmd: Option<String>,
    /// Launch directly without the supervisor
    #[usage(long)]
    exec: bool,
    /// Arguments forwarded to the coding tool
    #[usage(double_dash = "required")]
    claude_args: Vec<String>,
    #[usage(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommands)]
enum Command {
    /// Relay a coding tool's hook event to the supervisor
    #[usage(hide = true)]
    Hook {
        tool: String,
        event: Option<String>,
    },
    /// Distill a transcript delta into memories (spawned by the supervisor)
    #[usage(hide = true)]
    Review {
        #[usage(long, default = "claude")]
        tool: String,
        #[usage(long)]
        transcript: Option<PathBuf>,
        #[usage(long)]
        session: Option<String>,
        #[usage(long)]
        config_dir: PathBuf,
        #[usage(long)]
        cwd: Option<PathBuf>,
    },
    /// Install the memory relays into codex, pi, and opencode
    Relays {
        #[usage(subcommand)]
        command: RelaysCommand,
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
enum RelaysCommand {
    /// Write the relays into codex, pi, and opencode
    Install,
    /// Show each relay's installed state
    Status,
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
        Some(Command::Hook { tool, event }) => run_hook(&tool, event.as_deref()),
        Some(Command::Review {
            tool,
            transcript,
            session,
            config_dir,
            cwd,
        }) => run_review(&tool, transcript, session, &config_dir, cwd.as_deref()),
        Some(Command::Relays { command }) => match command {
            RelaysCommand::Install => relays::install_command(),
            RelaysCommand::Status => relays::status_command(),
        },
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
        None => {
            // The old flag wins when both are given, so existing scripts keep working
            let cmd = cli.claude_cmd.unwrap_or(cli.cmd);
            launch::run(&cmd, &cli.claude_args, cli.exec)
        }
    }
}

fn run_hook(tool: &str, event: Option<&str>) -> Result<()> {
    // Back-compat: an overlay from before the rename says `katami hook <Event>`,
    // so a lone positional is the event and the tool is claude.
    let (tool, event) = match event {
        Some(event) => (hook_protocol::Tool::parse(tool).unwrap_or_default(), event),
        None => (hook_protocol::Tool::Claude, tool),
    };
    hook_client::run(tool, event)
}

fn run_review(
    tool: &str,
    transcript: Option<PathBuf>,
    session: Option<String>,
    config_dir: &PathBuf,
    cwd: Option<&Path>,
) -> Result<()> {
    let tool = hook_protocol::Tool::parse(tool).unwrap_or_default();
    let source = match (tool, transcript, session) {
        (hook_protocol::Tool::Opencode, _, Some(session_id)) => {
            transcript::Source::Opencode { session_id }
        }
        (_, Some(path), _) => transcript::Source::File { tool, path },
        _ => anyhow::bail!("review needs --transcript for a file-based tool, or --session for opencode"),
    };
    reviewer::run(&source, config_dir, cwd)
}

