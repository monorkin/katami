# katami

A supervisor for coding agents that learns from your sessions, in Rust. It
wraps a launch in a transparent PTY, listens to the session through hooks,
and quietly builds a memory: what you told it about yourself, your projects,
and the people you work with comes back as context in future sessions. Works
with Claude Code, codex, pi, and opencode — one shared memory across all of
them. Inspired by the memory and curation systems in
[Hermes](https://github.com/NousResearch/hermes-agent). Linux only.

## Installing

With [mise](https://mise.jdx.dev):

```bash
mise use -g github:monorkin/katami                  # install it and put katami on your PATH
mise exec github:monorkin/katami -- katami --help   # or run it once without installing
```

mise hides releases younger than 24 hours by default (`minimum_release_age`).
If the latest release is fresher than that, pin the version instead:

```bash
mise use -g github:monorkin/katami@v0.2.3
```

On [Omarchy](https://omarchy.org):

```bash
omarchy-mise-install github:monorkin/katami katami
```

Then, once:

```bash
katami setup   # shell completion + the semantic-search model (~30MB)
```

## Usage

Everything after `katami` is the command to wrap — the first word picks the
tool, the rest is passed to it verbatim:

```bash
katami claude                             # wrap a claude session
katami claude --model opus                # forward args to claude
katami codex                              # wrap codex
katami pi                                 # or pi
katami opencode                           # or opencode
katami ax --account private -- --dangerously-skip-permissions   # any launcher that ends in claude
```

Run `katami` with no command to see the help. The rest are subcommands:

```bash
katami memory list                        # what it has learned
katami memory search "deploy process"     # hybrid BM25 + semantic search
katami memory show 12                     # one memory with its [[links]]
katami memory add "Title" "The fact." --entity project:/path
katami memory pull-models                 # enable semantic search (~30MB, once)
katami memory curate                      # consolidate and retire now

katami log -f                             # watch what the supervisor is doing
katami relays install                     # (re)install the codex/pi/opencode relays
katami relays status                      # show each relay's state

katami shell-completion install zsh       # bash, elvish, zsh, fish, nu, or powershell
```

### Wrapping codex, pi, and opencode

Every launch installs a small relay into codex, pi, and opencode, so a
session that shells out to another tool mid-run — asking codex for a review
from inside a claude session, say — feeds the same memory. The relays are
inert unless launched under `katami` (they gate on a socket env var), so an
installed relay never affects your normal, unsupervised sessions.

Two tool-specific notes:

- **codex** skips hooks it hasn't been told to trust. The first time the
  relay is installed, run `/hooks` inside codex once and trust the katami
  entries — otherwise codex silently ignores them. Your own codex hooks are
  preserved; katami's entries are merged in beside them.
- **pi and opencode** get a materialized extension/plugin file
  (`~/.pi/agent/extensions/katami-memory.ts`,
  `~/.config/opencode/plugins/katami-memory.js`). They're overwritten on each
  launch to stay current; don't edit them by hand.

## How it works

- **The supervisor.** `katami` spawns the tool under a pseudo-terminal and
  splices bytes verbatim — the terminal stream is never parsed, so nothing
  about the session's look or feel changes. Everything it learns arrives
  through hooks relayed to a unix socket. For claude, a per-launch settings
  overlay (passed via `--settings`, your own settings are never touched)
  registers the hooks; codex, pi, and opencode get their own persistent
  relays. A hook that can't reach the supervisor is a no-op — it can never
  wound the session.
- **Memory.** One shared SQLite store under `~/.local/share/katami/memory/`:
  observations, entity cards (one per person or project, rendered as markdown
  in `memory/cards/`), and `[[links]]` between them. On every prompt the
  supervisor runs a hybrid search — FTS5 BM25 fused with static
  [Model2Vec](https://github.com/MinishLab/model2vec) embeddings, all local,
  microseconds, zero model tokens — and injects the top memories plus
  one-line pointers to their linked neighbors.
- **The reviewer.** When a session stops, a detached background process feeds
  the transcript delta (your messages and the tool's conclusions, no tool
  noise) to `claude -p --model haiku` and asks what's worth remembering and
  whether a repeatable workflow deserves a skill. Its JSON is validated and
  applied in one transaction; a durable review queue means nothing is ever
  reviewed twice or lost to a crash.
- **The curator.** At most once a day, after a session ends: folds an
  entity's accumulated observations into its card, re-embeds anything the
  model missed, and archives generated skills nobody used for 90 days
  (configurable via `archive_skills_after_days` in
  `~/.local/share/katami/config.json`). Nothing is ever deleted — archived
  rows are the backup.
- **Generated skills.** Skills the reviewer proposes are materialized as
  `katami-<name>` entries in the claude config dir's `skills/` before each
  launch, and their usage is tracked through the PostToolUse hook to inform
  curation. A skills dir that's a symlink to shared territory is left alone.

## Building

```bash
cargo build --release              # → target/release/katami
sudo make install                  # → /usr/bin/katami
```

A local `make install` just needs the host toolchain.

Cutting a release (`make release`, or `make build-all` on its own)
cross-builds static musl binaries for amd64 and arm64, which needs
[`cross`](https://github.com/cross-rs/cross) and a running Docker or Podman:

```bash
cargo install cross
make release        # builds both targets, publishes the GitHub release
```

`Cross.toml` pins the `:main` cross images — the default pinned images ship a
glibc too old to run a current rustc's build scripts.

## License

MIT — see [LICENSE](LICENSE).
