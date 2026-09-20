//! The command line: what `katami` understands and what each command runs.
//! It lives in the library so a program built on katami can offer the same
//! commands — and has to, for the hidden ones: the supervisor re-runs its own
//! executable as `review`, `curate` and `hook`.

use anyhow::Result;
use std::path::{Path, PathBuf};
use usage::{Cli, Subcommands};

use crate::{
    completions, curator, embeddings, hook_client, hook_protocol, launch, link_cli, log_cli, memory,
    memory_cli, paths, relays, reranker, reviewer, setup, transcript, transfer, upgrade,
};

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
pub struct Cli {
    #[usage(subcommand)]
    pub command: Command,
}

#[derive(Subcommands)]
pub enum Command {
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
    /// Share memory with your other machines over Tailscale
    Link {
        #[usage(subcommand)]
        command: LinkCommand,
    },
    /// Listen for linked machines without a session running
    Serve,
    /// Show what the supervisor, reviewer, and curator have been doing
    Log {
        /// Number of recent lines to show
        #[usage(long, default = "50")]
        lines: usize,
        /// Keep printing new activity as it happens
        #[usage(long, short = 'f')]
        follow: bool,
    },
    /// Install shell completion and the semantic-search model
    Setup,
    /// Upgrade a mise install to the latest release
    Upgrade {
        /// Target a specific release instead of the latest
        version: Option<String>,
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
pub enum RelaysCommand {
    /// Write the relays into codex, pi, and opencode
    Install,
    /// Show each relay's installed state
    Status,
}

#[derive(Subcommands)]
pub enum LinkCommand {
    /// Link this machine to any one machine in the mesh; that one introduces the rest
    Up {
        /// Its hostname or IP
        host: String,
    },
    /// Take a machine out of the mesh, everywhere
    Break {
        /// Its name, as `katami link status` shows it
        name: String,
    },
    /// Let in a machine that isn't yours by Tailscale's word, by the code it shows
    Accept { code: String },
    /// Show who's linked, who's asking to pair, and what's waiting to merge
    Status,
}

#[derive(Subcommands)]
pub enum MemoryCommand {
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
    /// Show what a prompt would get injected, and what was judged irrelevant
    Judge { prompt: String },
    /// Show one memory with its links
    Show { id: String },
    /// Open a memory in $EDITOR
    Edit { id: String },
    /// Archive a memory so it stops being injected
    Archive { id: String },
    /// Bring an archived memory back
    Unarchive { id: String },
    /// List memories with their usage counts
    List {
        /// Include archived memories
        #[usage(long)]
        with_archived: bool,
        /// Show only archived memories
        #[usage(long)]
        archived: bool,
        /// Show only these kinds, comma-separated: observation, card, status, skill
        #[usage(long)]
        kinds: Option<String>,
        /// Order by these columns, comma-separated, each optionally followed by asc or desc: "last_used desc, uses desc"
        #[usage(long)]
        sort_by: Option<String>,
    },
    /// Write memories to a zip of markdown files: `all`, an id, or ids separated by commas
    Export {
        selection: String,
        /// Where to write the zip; defaults to katami-memories-<date>.zip here
        #[usage(long)]
        to: Option<PathBuf>,
    },
    /// Read memories from an exported zip; ones already here are kept and reported
    Import {
        path: PathBuf,
        /// Let the bundle's copy win when a memory is already here
        #[usage(long)]
        replace: bool,
        /// Have haiku write one memory out of the two when a memory is already here
        #[usage(long)]
        merge: bool,
    },
    /// Sync with every linked machine that can be reached, now
    Sync,
    /// Download the models that power semantic search and relevance judging
    PullModels,
    /// Consolidate observations into cards and archive unused skills now
    Curate,
}

#[derive(Subcommands)]
pub enum ShellCompletionCommand {
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
const SUBCOMMANDS: [&str; 12] = [
    "hook",
    "review",
    "relays",
    "curate",
    "link",
    "log",
    "memory",
    "serve",
    "setup",
    "upgrade",
    "shell-completion",
    "help",
];

pub fn main() {
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

pub fn run(cli: Cli) -> Result<()> {
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
        Command::Curate { config_dir } => curator::run(&config_dir, curator::Reason::Scheduled),
        Command::Log { lines, follow } => log_cli::print(lines, follow),
        Command::Link { command } => match command {
            LinkCommand::Up { host } => link_cli::up(&host),
            LinkCommand::Break { name } => link_cli::break_with(&name),
            LinkCommand::Accept { code } => link_cli::accept(&code),
            LinkCommand::Status => link_cli::status(),
        },
        Command::Serve => link_cli::serve(),
        Command::Setup => setup::run(),
        Command::Upgrade { version } => upgrade::run(version.as_deref()),
        Command::Memory { command } => match command {
            MemoryCommand::Add {
                title,
                body,
                entity,
                link,
                card,
            } => memory_cli::add(&title, &body, entity, link, card),
            MemoryCommand::Search { query } => memory_cli::search(&query),
            MemoryCommand::Judge { prompt } => memory_cli::judge(&prompt),
            MemoryCommand::Show { id } => memory_cli::show(&id),
            MemoryCommand::Edit { id } => memory_cli::edit(&id),
            MemoryCommand::Archive { id } => memory_cli::archive(&id),
            MemoryCommand::Unarchive { id } => memory_cli::unarchive(&id),
            MemoryCommand::List {
                with_archived,
                archived,
                kinds,
                sort_by,
            } => {
                let filter = if archived {
                    memory::ListFilter::ArchivedOnly
                } else if with_archived {
                    memory::ListFilter::All
                } else {
                    memory::ListFilter::Active
                };
                memory_cli::list(filter, kinds.as_deref(), sort_by.as_deref())
            }
            MemoryCommand::Export { selection, to } => transfer::export(&selection, to),
            MemoryCommand::Import { path, replace, merge } => {
                let on_collision = match (replace, merge) {
                    (true, true) => anyhow::bail!("--replace and --merge settle collisions differently — pick one"),
                    (true, false) => transfer::OnCollision::Replace,
                    (false, true) => transfer::OnCollision::Merge,
                    (false, false) => transfer::OnCollision::Skip,
                };
                transfer::import(&path, on_collision, &paths::claude_config_home())
            }
            MemoryCommand::Sync => link_cli::sync(),
            MemoryCommand::PullModels => embeddings::pull().and_then(|_| reranker::pull()),
            MemoryCommand::Curate => curator::run(&paths::claude_config_home(), curator::Reason::Asked),
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

