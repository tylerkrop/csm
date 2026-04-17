# csm — Copilot Session Manager

A CLI tool for managing [GitHub Copilot](https://github.com/features/copilot) coding sessions inside [Zellij](https://zellij.dev). Each session gets its own Git worktree, branch, and Zellij terminal session with Copilot automatically started.

## Features

- **One command to start coding** — `csm run <name>` creates a branch, worktree, Zellij session, and launches Copilot in one step.
- **Session lifecycle management** — start, stop, attach, remove, restore, and rename sessions.
- **Git worktree isolation** — each session works in its own worktree so you can run multiple sessions in parallel without conflicts.
- **Persistent state** — session metadata is stored in a local SQLite database (`~/.csm/sessions.db`).
- **Copilot auto-resume** — sessions are tied to a stable UUID so Copilot can resume context across restarts.

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) (edition 2024)
- [Zellij](https://zellij.dev/documentation/installation)
- [GitHub Copilot CLI](https://docs.github.com/en/copilot)
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

Creates a Git branch (`tylerkrop/<name>`) and worktree, starts a Zellij session, and injects the Copilot resume command. The worktree is created under `~/.csm/worktrees/<repo>/<repo>-<shortcode>`, where `<shortcode>` is the first 8 hex characters of the session's UUID.

Session names must be alphanumeric and may contain `-` or `_`.

### List sessions

```sh
csm list        # active sessions
csm list -a     # include removed sessions
```

Shows the session shortcode, name, repository, branch, status (running/exited/stopped/removed), and last-used time. Sessions are sorted by status (running first) then most recently used.

### Attach to a running session

```sh
csm attach <name>
```

`<name>` may be either the session name or a unique prefix of its UUID shortcode. This applies to every command that takes a session identifier (`attach`, `start`, `stop`, `remove`, `restore`, `rename`).

### Start a stopped session

```sh
csm start <name>
```

Recreates the Zellij session and re-injects the Copilot command.

### Stop a session

```sh
csm stop <name>
```

Kills the Zellij session but keeps the worktree and branch intact.

### Remove sessions

```sh
csm remove <name>...       # soft remove (keeps branch)
csm remove <name>... -f    # destroy worktree and branch
```

### Restore a removed session

```sh
csm restore <name>
```

Recreates the worktree from the existing branch.

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

1. `csm run` finds the current Git repo, creates a new branch (prefixed with `tylerkrop/`) and worktree, and inserts a session record into SQLite.
2. A Zellij session is started in the worktree directory, named after the first 8 hex characters of the session's UUID.
3. A background task waits for Zellij to be ready, then types the Copilot resume command into the first pane.
4. On detach, the `last_used_at` timestamp is updated. If the user quit Zellij entirely (e.g. `Ctrl+q`), the exited Zellij session is cleaned up so it shows as `stopped` in `csm list`.
5. Sessions can be stopped, restarted, removed, or restored independently — the underlying Git branch persists until explicitly destroyed with `remove -f`.

The SQLite database is opened in WAL mode with a 5-second busy timeout, so multiple `csm` invocations can run concurrently without `SQLITE_BUSY` errors.

## Data Storage

All data lives under `~/.csm/`:

```
~/.csm/
├── sessions.db                              # SQLite database
└── worktrees/
    └── <repo>/
        └── <repo>-<shortcode>/              # Git worktree
```

## License

This project does not yet have a license file. All rights reserved by the author until one is added.
