# agent

A supervisor for coding agents that learns from your sessions, in Rust. It
wraps a launch in a transparent PTY, listens to the session through hooks,
and quietly builds a memory: what you told it about yourself, your projects,
and the people you work with comes back as context in future sessions. Works
with Claude Code, codex, pi, and opencode — one shared memory across all of
them. Inspired by the memory and curation systems in
[Hermes](https://github.com/NousResearch/hermes-agent). Linux only.

## Installing

```bash
git clone git@github.com:monorkin/agent.git
cd agent
cargo build --release
install -Dm755 target/release/agent ~/.local/bin/agent
```

Any directory on your `PATH` works in place of `~/.local/bin`. Then, once:

```bash
agent memory pull-models   # ~30MB, enables semantic search
```

## Usage

```bash
agent                                    # wrap a plain claude session
agent -- --model opus                    # forward args to the tool
agent --cmd codex                        # wrap codex instead
agent --cmd pi                           # or pi
agent --cmd opencode                     # or opencode
agent --cmd "ax run --account work --"   # wrap another launcher (ends in claude)
agent --exec --                          # skip the supervisor entirely

agent memory list                        # what it has learned
agent memory search "deploy process"     # hybrid BM25 + semantic search
agent memory show 12                     # one memory with its [[links]]
agent memory add "Title" "The fact." --entity project:/path
agent memory pull-models                 # enable semantic search (~30MB, once)
agent memory curate                      # consolidate and retire now

agent log -f                             # watch what the supervisor is doing
agent relays install                     # (re)install the codex/pi/opencode relays
agent relays status                      # show each relay's state

agent shell-completion install zsh       # bash, elvish, zsh, fish, nu, or powershell
```

`--cmd` is split on whitespace — no shell quoting. Wrap anything fancier in a
script. (`--claude-cmd` is a deprecated alias.)

### Wrapping codex, pi, and opencode

Every launch installs a small relay into codex, pi, and opencode, so a
session that shells out to another tool mid-run — asking codex for a review
from inside a claude session, say — feeds the same memory. The relays are
inert unless launched under `agent` (they gate on a socket env var), so an
installed relay never affects your normal, unsupervised sessions.

Two tool-specific notes:

- **codex** skips hooks it hasn't been told to trust. The first time the
  relay is installed, run `/hooks` inside codex once and trust the agent
  entries — otherwise codex silently ignores them. Your own codex hooks are
  preserved; agent's entries are merged in beside them.
- **pi and opencode** get a materialized extension/plugin file
  (`~/.pi/agent/extensions/agent-memory.ts`,
  `~/.config/opencode/plugins/agent-memory.js`). They're overwritten on each
  launch to stay current; don't edit them by hand.

Since `agent` is a fairly generic name for a binary, check for PATH
collisions (`type -a agent`) before installing; rename the binary in
`Cargo.toml` under `[[bin]]` if it bites.

## How it works

- **The supervisor.** `agent` spawns claude under a pseudo-terminal and
  splices bytes verbatim — the terminal stream is never parsed, so nothing
  about the session's look or feel changes. Everything it learns arrives
  through Claude Code hooks: a per-launch settings overlay (passed via
  `--settings`, your own settings are never touched) registers hooks that
  relay their event to the supervisor over a unix socket and print the reply.
  A hook that can't reach the supervisor prints `{}` and exits 0 — it can
  never wound the session.
- **Memory.** One shared SQLite store under `~/.local/share/agent/memory/`:
  observations, entity cards (one per person or project, rendered as markdown
  in `memory/cards/`), and `[[links]]` between them. On every prompt the
  supervisor runs a hybrid search — FTS5 BM25 fused with static
  [Model2Vec](https://github.com/MinishLab/model2vec) embeddings, all local,
  microseconds, zero model tokens — and injects the top memories plus
  one-line pointers to their linked neighbors.
- **The reviewer.** When a session stops, a detached background process feeds
  the transcript delta (your messages and claude's conclusions, no tool
  noise) to `claude -p --model haiku` and asks what's worth remembering and
  whether a repeatable workflow deserves a skill. Its JSON is validated and
  applied in one transaction; a byte cursor per transcript means nothing is
  ever reviewed twice.
- **The curator.** At most once a day, after a session ends: folds an
  entity's accumulated observations into its card, re-embeds anything the
  model missed, and archives generated skills nobody used for 90 days
  (configurable via `archive_skills_after_days` in
  `~/.local/share/agent/config.json`). Nothing is ever deleted — archived
  rows are the backup.
- **Generated skills.** Skills the reviewer proposes are materialized as
  `agent-<name>` entries in the claude config dir's `skills/` before each
  launch, and their usage is tracked through the PostToolUse hook to inform
  curation. A skills dir that's a symlink to shared territory is left alone.

## Building

Static musl release builds need `musl-tools` (Debian) or `musl` (Arch) for
the C pieces (bundled SQLite, ring).

## License

MIT — see [LICENSE](LICENSE).
