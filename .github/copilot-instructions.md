# Copilot instructions for csm

csm (Copilot Session Manager) is a Rust CLI that manages GitHub Copilot coding
sessions inside [Zellij](https://zellij.dev). Each session gets its own Git
branch, Git worktree, and Zellij session with Copilot auto-started. The binary
is named `csm` (see `[[bin]]` in `Cargo.toml`).

## Build, test, and lint

- Build: `cargo build` (release: `cargo build --release`).
- Test: `cargo test` runs the full suite (inline unit tests).
- Run a single test: `cargo test parse_running_and_exited`, or scope to a
  module with `cargo test zellij::tests`.
- Lint: `cargo clippy`. Format: `cargo fmt`.
- Requires Rust edition 2024 (toolchain 1.85+).

Tests are inline `#[cfg(test)] mod tests` blocks, not a top-level `tests/`
directory. Pure logic is deliberately extracted into free functions so it can
be unit tested without spawning external binaries (e.g. `parse_list_sessions`
in `zellij.rs`, `shortest_unique_prefixes_within` in `display.rs`,
`repo_name` in `git.rs`). When adding behavior, prefer this pattern over
testing through `Command`.

## Architecture

The flow is: `main.rs` (clap CLI) dispatches to one async function per
subcommand in `commands.rs`, which orchestrates four lower-level modules.

- `main.rs` defines the `clap` `Commands` enum (subcommands, aliases, args)
  and maps each variant to a `commands::*` call. `#[tokio::main]` async entry.
- `commands.rs` is the orchestration layer holding all business logic
  (run/start/attach/stop/remove/list/restore/rename). It coordinates the DB,
  git, and zellij modules. Shared helpers and constants live at the top.
- `db.rs` opens the SQLite connection and creates the schema. `entity/session.rs`
  is the single SeaORM entity (`Model`/`ActiveModel`) for the `sessions` table.
- `git.rs` wraps `git` worktree/branch operations via `std::process::Command`.
- `zellij.rs` wraps the `zellij` CLI: query session state, start/kill/cleanup
  sessions, write the per-session layout (`~/.csm/layouts/<uuid>.kdl`, git vs
  no-git variants), the shared config (`config.kdl`), and the copilot launcher
  script (`launch-copilot.sh`). The layout has named tabs: `ai` (focused, runs
  the launcher), `git` (gitui, omitted with no repo), and `edit` (nvim).
- `display.rs` is pure formatting: shortcodes, colors, relative times, status
  ranking. No I/O side effects beyond reading whether stdout is a TTY.
- `interactive.rs` is a self-contained `crossterm` fullscreen multi-select
  picker used by `csm remove -i`.

There is no daemon or server. csm shells out to external binaries (`git`,
`zellij`, and within the layout `gitui`/`nvim`) rather than using libraries.

## Key conventions

- A session's stable identity is its `copilot_uuid`. The Zellij session name is
  always the first 8 hex characters of that UUID (`display::short_uuid`), not
  the human name, so renaming a session never touches Zellij.
- The DB primary key is the human `name`, but every command that takes a
  session identifier resolves it through `resolve_session`, which accepts an
  exact name OR a unique UUID-shortcode prefix. Reuse this helper; do not look
  sessions up by name directly in new commands.
- Git branches are always prefixed with `tylerkrop/` (`BRANCH_PREFIX`).
- All persistent state lives under `~/.csm/`: `sessions.db`, `config.kdl`,
  `launch-copilot.sh`, `layouts/<uuid>.kdl`, `markers/<uuid>`, and
  `worktrees/<repo>/<repo>-<shortcode>/`.
- Two status vocabularies: the DB stores only `active`/`removed`
  (`STATUS_ACTIVE`/`STATUS_REMOVED`); the user-facing status
  (running/exited/stopped/removed) is derived at display time by querying live
  Zellij state via `zellij::State`. Don't persist the display statuses.
- Defense-in-depth checks are intentional: UUIDs are re-parsed
  (`zellij::validate_uuid`) before being written into a layout file or marker
  path, and constructed worktree paths are verified to start with `~/.csm`
  before use. Preserve these guards.
- SQLite is opened in WAL mode with a 5s busy timeout so concurrent `csm`
  invocations don't hit `SQLITE_BUSY`. Keep new DB work compatible with that.
- Mutating commands refresh `last_used_at` (via `now_str()`), and that field
  drives list/picker ordering alongside `status_rank`.
- Failures during session creation roll back: `run` deletes the DB row, reaps
  the layout/marker via `zellij::cleanup_session_files`, and removes the
  worktree if startup fails. Follow this create-then-cleanup shape.
- Orphaned per-session files are swept on `rm`: after removals, `commands::rm`
  calls `zellij::prune_orphans` with every remaining session UUID, deleting
  layout `.kdl` files and markers with no DB row. It matches both the full-UUID
  and older shortcode (`<shortcode>.kdl`) filename schemes, so files that
  `cleanup_session_files` (UUID-only) can't match are still reaped.
- `run` has three modes: normal (new branch + worktree), non-repo (skips
  branch/worktree, runs Copilot in the cwd), and `--here` (same, but forced even
  inside a repo). If a session name is taken, `run` appends a numeric suffix
  (`<name>-2`, …) rather than erroring; the branch keeps the requested name.
- The `ai` tab runs `~/.csm/launch-copilot.sh <uuid>` as a zellij command pane
  (not an injected keystroke), so zellij owns copilot and offers Enter-to-rerun
  on exit, like the `git`/`edit` tabs. The launcher self-selects `copilot --name`
  on a session's first launch and `copilot --resume` afterward, keyed by a marker
  file (`~/.csm/markers/<uuid>`) written before the first `--name` launch so a
  killed session never spawns a duplicate. `run` starts a session with
  `resume=false` (let the launcher create the marker); `start`/`restore` pass
  `resume=true`, which calls `zellij::ensure_marker` up front so pre-existing
  sessions resume. Preserve this marker contract when changing session startup.
- Source uses `// ── Section ──` comment banners to group helpers vs commands.
