# csm — Copilot Session Manager

A CLI tool for managing [GitHub Copilot](https://github.com/features/copilot) coding sessions inside [Zellij](https://zellij.dev). Sessions can run in local Git worktrees or remote GitHub Codespaces, with Copilot started automatically.

## Features

- **One command to start coding** — `csm run <name>` creates a branch, worktree, Zellij session, and launches Copilot in one step.
- **Session lifecycle management** — start, stop, attach, remove, restore, and rename sessions.
- **Git worktree isolation** — each session works in its own worktree so you can run multiple sessions in parallel without conflicts.
- **GitHub Codespaces support** - create from a repository's default branch and run Copilot inside a persistent remote tmux session.
- **Persistent state** — session metadata is stored in a local SQLite database (`~/.csm/sessions.db`).
- **Copilot auto-resume** — sessions are tied to a stable UUID so Copilot can resume context across restarts.

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) (edition 2024)
- [Zellij](https://zellij.dev/documentation/installation)
- [GitHub Copilot CLI](https://docs.github.com/en/copilot)
- [GitHub CLI](https://cli.github.com/) (required for Codespace sessions)
- tmux in the Codespace (installed automatically with `apt-get` when available)
- [gitui](https://github.com/gitui-org/gitui) (used by the `git` tab in the default layout)
- [Neovim](https://neovim.io) (used by the `edit` tab in the default layout)
- Git

## Installation

```sh
cargo install --path .
```

## Usage

### Create and start a new session

```sh
csm run <name>
```

Creates a Git branch (`tylerkrop/<name>`) and worktree, starts a Zellij session, and launches Copilot with the session's stable UUID. The worktree is created under `~/.csm/worktrees/<repo>/<repo>-<shortcode>`, where `<shortcode>` is the first 8 hex characters of the session's UUID.

If the current repository is on a default branch (`main` or `master`), csm runs `git pull` first so the new worktree branches from up-to-date history. If you run `csm run` in a directory that is not a Git repository, csm skips branch/worktree creation and starts Copilot directly in the current directory.

If the branch `tylerkrop/<name>` already exists, csm prompts for confirmation before resuming it in a new worktree, so you never silently reuse old branch history.

If the session name is already in use by an active session, csm appends a numeric suffix (`<name>-2`, `<name>-3`, …) instead of erroring. This lets you reuse the same branch name across different repositories; the branch keeps the requested name while the session name is disambiguated. (Most of the time you connect using the UUID shortcode anyway.)

Pass `--here` to skip worktree creation and run Copilot directly in the current directory, even inside a Git repository. This is handy for hobby projects where you don't want to merge from other branches. The `git` tab (gitui) is omitted automatically when the working directory is not a Git repository.

### Create a session in GitHub Codespaces

```sh
csm run <name> --codespace
csm run <name> --cs
```

Codespace sessions are always created from the current GitHub repository's default branch so they can use available prebuilds. `csm` hands creation to `gh codespace create`, which may prompt for a dev container, machine type, or additional permissions. Local uncommitted and unpushed changes are not copied into the Codespace.

After creation, csm opens a local Zellij session with one `cs` tab. That tab connects with `gh codespace ssh` and attaches to a remote tmux session. Its first window is named `ai` and runs `copilot --autopilot --yolo`; create another tmux window when you want to create or switch to a task branch manually.

Detaching or losing SSH leaves tmux and Copilot running in the Codespace. `csm stop` also stops the Codespace to avoid continued billing. A soft `csm remove` stops and retains the Codespace, `csm restore` reconnects it, and `csm remove -f` deletes it.

csm records the GitHub account that created each Codespace. If you later switch accounts with `gh auth switch`, switch back to the recorded account before starting, stopping, restoring, or deleting that session.

If tmux is missing, csm installs it with `apt-get` using root or passwordless `sudo`. Custom images without `apt-get` must provide tmux in their dev container configuration.

If Copilot CLI is missing, csm installs it with the official `https://gh.io/copilot-install` script. This requires `curl` and `bash` in the Codespace.

Session names must be alphanumeric and may contain `-` or `_`.

### List sessions

```sh
csm list        # active sessions
csm list -a     # include removed sessions
```

Shows the session shortcode, name, repository, branch, status (running/exited/stopped/removed), and last-used time. Codespace repositories have an `@cs` suffix and their status includes both local Zellij and remote Codespace state, such as `running/available`. Sessions are sorted by status (running first) then most recently used.

### Attach to a running session

```sh
csm attach <name>
```

`<name>` may be either the session name or a unique prefix of its UUID shortcode. This applies to every command that takes a session identifier (`attach`, `start`, `stop`, `remove`, `restore`, `rename`).

### Start a stopped session

```sh
csm start <name>
```

Recreates the Zellij session and resumes Copilot with `--resume`, so the
session's prior conversation context is restored rather than started fresh.
For a Codespace session, this also starts the Codespace and reattaches tmux.

### Stop a session

```sh
csm stop <name>
```

Kills the Zellij session but keeps the worktree and branch intact. For a
Codespace session, it also stops the Codespace.

### Remove sessions

```sh
```sh
csm remove <name>...              # soft remove (keeps branch)
csm remove <name>... -f           # destroy worktree and branch
csm remove -i                     # interactive multi-select picker
csm remove -i -f                  # interactive picker, destroying selected sessions
csm remove --older-than 30        # soft-remove every session inactive for 30+ days
csm remove --older-than 30 -f     # destroy every session inactive for 30+ days
```

`--older-than <DAYS>` selects every non-removed session whose last-used time is at least `<DAYS>` days ago. It can be combined with explicit names and with `-f`.

In interactive mode (`-i`), `csm` opens a fullscreen picker listing the active
sessions. By default already-removed sessions are hidden; press `a` to toggle
them into the list (combine with `-f` to permanently destroy them).

| Key                | Action                                                  |
|--------------------|---------------------------------------------------------|
| `j` / `↓`          | Move cursor down                                        |
| `k` / `↑`          | Move cursor up                                          |
| `g` / `G`          | Jump to top / bottom                                    |
| `space`            | Toggle selection of the highlighted session             |
| `a`                | Toggle showing already-removed sessions (mirrors `ls -a`) |
| `enter`            | Submit selected sessions (then confirm with `y`)        |
| `/`                | Enter search mode (live filter as you type)             |
| `enter` (search)   | Return to select mode, keeping the filter               |
| `esc`              | Clear the filter (works from select mode too)           |
| `y` (confirm)      | Confirm removal                                         |
| any other (confirm)| Cancel the prompt and return to select mode             |
| `ctrl-c`           | Cancel without removing anything                        |

If no sessions are explicitly selected with `space`, pressing `enter` removes
the highlighted session only. Selections are preserved across filter changes,
so a hidden session that was selected before filtering will still be removed.
A `y/N` confirmation step protects against accidental double-Enter (e.g. after
finishing a search).

### Restore a removed session

```sh
csm restore <name>
```

Recreates the worktree from the existing branch, or starts and reconnects a
soft-removed Codespace.

### Rename a session

```sh
csm rename <old> <new>
```

## Command Aliases

| Command   | Aliases  |
|-----------|----------|
| `run`     | `r`      |
| `start`   | `s`      |
| `attach`  | `a`      |
| `stop`    | `k`      |
| `remove`  | `rm`     |
| `list`    | `ls` `ps`|
| `rename`  | `mv`     |

## How It Works

1. Local `csm run` finds the current Git repo, creates a new branch (prefixed with `tylerkrop/`) and worktree, and inserts a session record into SQLite. With `--codespace`, csm resolves the repository's default branch and delegates creation to `gh codespace create` instead.
2. A Zellij session is started in the worktree directory, named after the first 8 hex characters of the session's UUID. The session uses a per-session layout (written to `~/.csm/layouts/<uuid>.kdl`, with a no-Git variant when there is no Git repo) with named tabs: `ai` (focused), `git` (runs `gitui`, omitted outside a Git repo), and `edit` (runs `nvim`). A config (written to `~/.csm/config.kdl`) enables the simplified ASCII UI (`simplified_ui`) and removes pane frames/borders (`pane_frames false`).
3. The `ai` tab runs a small launcher script (`~/.csm/launch-copilot.sh <uuid>`) as a Zellij command pane, so Zellij owns the Copilot process just like `gitui`/`nvim` in the other tabs: when Copilot exits you can press Enter to re-run it. The launcher records a marker under `~/.csm/markers/<uuid>` on a session's first launch and uses it to pick the right flag. It runs `copilot --session-id=<uuid>` the first time, then `copilot --resume=<uuid>` on every re-run, so re-running resumes the same conversation.
4. On detach, the `last_used_at` timestamp is updated. If the user quit Zellij entirely (e.g. `Ctrl+q`), the exited Zellij session is cleaned up so it shows as `stopped` in `csm list`.
5. Sessions can be stopped, restarted, removed, or restored independently — the underlying Git branch persists until explicitly destroyed with `remove -f`.

For Codespace sessions, csm copies a remote launcher to a Codespace-specific path under `/tmp`. The launcher creates or reattaches a UUID-scoped tmux session, restores the `ai` window if it exited, and uses a remote marker under `~/.csm/markers` to select `copilot --session-id` on first launch or `copilot --resume` later.

The SQLite database is opened in WAL mode with a 5-second busy timeout, so multiple `csm` invocations can run concurrently without `SQLITE_BUSY` errors.

## Data Storage

All data lives under `~/.csm/`:

```
~/.csm/
├── sessions.db                              # SQLite database
├── config.kdl                               # Zellij config (ASCII UI, no pane frames)
├── launch-copilot.sh                        # Copilot launcher (picks --session-id/--resume)
├── launch-codespace.sh                      # Script copied into Codespaces
├── layouts/
│   └── <uuid>.kdl                           # Per-session Zellij layout
├── markers/
│   └── <uuid>                               # Marks that a session has been created
└── worktrees/
    └── <repo>/
        └── <repo>-<shortcode>/              # Git worktree
```

## License

This project does not yet have a license file. All rights reserved by the author until one is added.
