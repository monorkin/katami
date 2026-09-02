# katami

A supervisor for coding agents (claude, codex, pi, opencode) that learns from
your sessions and injects the memory back as context. Rust, Linux-only,
distributed through mise.

## The one rule that will bite you

**Never run anything against the real memory store.** It lives at
`~/.local/share/katami/` and holds real memories. Every test, every manual
run, every experiment goes through a throwaway `XDG_DATA_HOME`:

```bash
export XDG_DATA_HOME=$(mktemp -d)
```

katami derives its whole data dir from that, so the store, models, logs, and
launches all land in the sandbox. If embeddings are needed, symlink the real
model dir in: `ln -s ~/.local/share/katami/models $XDG_DATA_HOME/katami/models`.
Relays and codex hooks also write into real config — sandbox those too with
`CODEX_HOME`, `XDG_CONFIG_HOME`, and `PI_CODING_AGENT_DIR`.

## How the pieces fit

- **`supervisor.rs`** spawns the wrapped tool under a PTY, splices bytes
  verbatim (the terminal stream is never parsed), and serves a unix socket.
  Everything katami learns arrives as hook events, not from the terminal.
- **Relays** carry the integration into each tool. claude gets a per-launch
  `--settings` overlay (`overlay.rs`, `launch.rs`) so its real settings stay
  untouched; codex/pi/opencode get persistent relay files installed by
  `relays.rs` (the TS/JS sources are embedded and materialized). Every relay
  gates on `KATAMI_HOOK_SOCKET`, so it's inert outside a supervised session.
- **`hook_client.rs`** is the thin relay claude and codex run; it formats the
  supervisor's canonical `{"context": …}` reply into each tool's hook shape.
- **`memory.rs`** is one SQLite store: observations, cards, status, links,
  deliveries, evidence, the review queue. Schema changes bump `user_version`
  and add a migration step — see `migrate()`.
- **`reviewer.rs`** runs after a session stops: transcript delta → durable
  `review_chunks` queue → `distiller.rs` (haiku via `claude -p`) → validated
  JSON applied in one transaction. **The reviewer and curator always use
  `claude -p` for every tool's transcripts**, so claude must be installed.
- **`transcript.rs`** + `transcript_{codex,pi,opencode}.rs` parse each tool's
  session format behind one `Source` abstraction. Each parser structurally
  filters katami's own injected context so the store never eats its output.
- **`curator.rs`** consolidates observations into cards and retires unused
  skills, at most once a day.

## Style

Match the surrounding code, which follows the ax conventions:

- Flat `src/*.rs`, no submodule directories.
- `anyhow` everywhere; error messages are advice with an em dash and a next
  step ("no memory with id 5 — see `katami memory list`").
- Every module opens with a `//!` doc explaining *why*, not mechanics. Almost
  no inline comments — a comment usually means the code should read better.
- Prose-like names, `|it|` closures, let-chains and let-else where they read
  well. Sync only — threads and mpsc, no async runtime.
- Named enums over stringly-typed fields (`Kind`, `Tool`, not `&str`).
- Tests live inline in `#[cfg(test)] mod tests`; fixtures under the scratch
  temp dir, never the real store.

## Commands

```bash
cargo build --release      # → target/release/katami
cargo test                 # inline unit tests
make install               # → /usr/bin/katami (host toolchain)
make release               # cross-build musl amd64/arm64, publish GitHub release
```

`make release` needs `cross` and Docker/Podman; `Cross.toml` pins the `:main`
images because the default pinned ones ship a glibc too old for a current
rustc.

## CLI shape

`katami <tool> …` supervises whatever follows — the first word picks the
adapter (`katami claude`, `katami codex`, `katami ax --account x -- …`). Bare
`katami` prints help. Reserved subcommands (`memory`, `log`, `relays`,
`setup`, `upgrade`, `hook`, `review`, `curate`, `shell-completion`) are listed
in `SUBCOMMANDS` in `main.rs`; anything else in the first position is a
launcher.
