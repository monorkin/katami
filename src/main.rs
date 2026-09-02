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
#[usage(
    bin = "katami",
    version,
    unknown_flags = "error",
    completion,
    arg_required_else_help,
    after_help = "Wrap a tool by running it after katami:\n  katami claude\n  katami codex\n  katami ax --account private -- --dangerously-skip-permissions"
)]
struct Cli {
    #[usage(subcommand)]
    command: Command,
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

/// The reserved words that name a katami subcommand rather than a coding tool
/// to supervise. Anything else in the first position is a launcher.
const SUBCOMMANDS: [&str; 8] = [
    "hook",
    "review",
    "relays",
    "curate",
    "log",
    "memory",
    "shell-completion",
    "help",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first() {
        Some(first) if is_launcher(first) => launch::run(&args),
        _ => run(Cli::parse()),
    };
    if let Err(error) = result {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

/// The first word is a launcher to supervise unless it's a flag (`-`/`--`), a
/// usage-rs internal (`__complete_word__`), or a reserved subcommand.
fn is_launcher(word: &str) -> bool {
    !word.starts_with('-') && !word.starts_with('_') && !SUBCOMMANDS.contains(&word)
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Hook { tool, event } => run_hook(&tool, event.as_deref()),
        Command::Review {
            tool,
            transcript,
            session,
            config_dir,
            cwd,
        } => run_review(&tool, transcript, session, &config_dir, cwd.as_deref()),
        Command::Relays { command } => match command {
            RelaysCommand::Install => relays::install_command(),
            RelaysCommand::Status => relays::status_command(),
        },
        Command::Curate { config_dir } => curator::run(&config_dir),
        Command::Log { lines, follow } => log_cli::print(lines, follow),
        Command::Memory { command } => match command {
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
        Command::ShellCompletion { command } => match command {
            ShellCompletionCommand::Print { shell } => completions::print(&shell),
            ShellCompletionCommand::Install { shell } => completions::install(&shell),
        },
    }
}

fn run_hook(tool: &str, event: Option<&str>) -> Result<()> {
    // claude's settings overlay registers `katami hook <Event>` with no tool,
    // so a lone positional is the event and the tool is claude. codex names
    // both: `katami hook codex <Event>`.
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

