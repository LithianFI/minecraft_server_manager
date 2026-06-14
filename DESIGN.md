# Minecraft Server Manager — Design Document

## Overview

A Rust binary that manages multiple Minecraft server instances and serves a local web UI. The binary runs in a terminal (or as a systemd service), auto-opens `http://localhost:8080` in the browser, and streams live updates via Server-Sent Events. Targets a small group of friends who rotate between 3–4 modded (NeoForge) servers, with Discord integration for remote control.

---

## Goals

1. Easy start/stop/switch between multiple server instances
2. Manage mod updates via Modrinth
3. Scheduled and manual backups with retention policy
4. Discord bot for remote control (restart, switch, player info)
5. Live server log streaming in the UI
6. Resource monitoring (RAM, TPS)

**Out of scope for v1:**
- Instance creation from scratch (users add instances manually)
- Remote/cloud backups
- Multi-machine management
- CurseForge mod support

---

## Technology Stack

| Layer | Choice | Rationale |
|---|---|---|
| Backend | Rust | Performance, safe concurrency, good async story |
| HTTP server | `axum` | Ergonomic, SSE support built-in, tokio-native |
| Frontend | Plain HTML + CSS + JS | No build step, no Node/npm, embedded via `include_str!` |
| Live updates | Server-Sent Events | One-directional server→browser stream; browser auto-reconnects |
| Discord bot | `poise` + `serenity` crates | Runs as a tokio task in the same process |
| Mod API | Modrinth API | NeoForge-native, no API key needed for reads |
| Config | TOML | Human-readable, easy to hand-edit |
| Async runtime | Tokio | Shared by axum, serenity, and all background tasks |

### Development workflow

```
cargo run
  → serves http://localhost:8080
  → browser opens automatically
```

The three UI files (`src/ui/index.html`, `style.css`, `app.js`) are embedded at compile time via `include_str!` and served as static routes. Edit a UI file, `cargo run` again — no separate build step ever needed.

---

## Architecture

```
msm binary (single process)
├── axum HTTP server (:8080)
│   ├── GET  /                      → index.html (include_str!)
│   ├── GET  /style.css             → style.css  (include_str!)
│   ├── GET  /app.js                → app.js     (include_str!)
│   ├── GET  /events                → SSE stream (push events)
│   ├── GET  /api/instances         → list all instances + state
│   ├── GET  /api/instances/:id/logs
│   ├── POST /api/instances         → add instance
│   ├── POST /api/instances/:id/start
│   ├── POST /api/instances/:id/stop
│   ├── POST /api/instances/:id/switch
│   ├── POST /api/instances/:id/backup
│   └── POST /api/instances/:id/cmd → write to server stdin
│
├── AppState (Arc<>>)
│   ├── instances: RwLock<HashMap<String, InstanceState>>
│   │     └── InstanceState holds: config, status, players, log_buffer
│   ├── processes: Mutex<HashMap<String, ProcessHandle>>
│   │     └── ProcessHandle holds: stdin_tx (mpsc channel to server stdin)
│   └── log_tx: broadcast::Sender<WsEvent>
│
├── instance   — process lifecycle, state machine, log parsing
├── sse        — SSE handler: sends init snapshot + streams broadcast events
├── api        — REST route handlers
├── backup     — tar+zstd archives, retention, scheduling  (Phase 2)
├── mod_mgr    — Modrinth API client, manifest diffing     (Phase 3)
├── metrics    — RAM from /proc, TPS from log parsing      (Phase 5)
└── discord    — tokio task, Arc<AppState> clone           (Phase 4)
```

**SSE push events** (JSON, server → browser):

| Event | Payload |
|---|---|
| `init` | `{ instances: InstanceInfo[] }` — sent on every (re)connect |
| `log_history` | `{ instance_id, lines: LogLine[] }` — buffered logs on connect |
| `log_line` | `{ instance_id, line, timestamp }` |
| `state_changed` | `{ instance_id, status }` |
| `player_joined` | `{ instance_id, player }` |
| `player_left` | `{ instance_id, player }` |
| `backup_done` | `{ instance_id, path, size_bytes }` |
| `metrics` | `{ instance_id, ram_mb, tps }` |

The browser uses `EventSource` (auto-reconnect built-in). Commands flow the other way as REST POSTs. The Discord bot holds a clone of `Arc<AppState>` and mutates it directly — no IPC needed.

---

## Directory Structure

```
~/.local/share/msm/
├── config.toml                  # global config
├── instances/
│   ├── survival/
│   │   ├── msm.toml             # instance config
│   │   ├── mods.lock.toml       # installed mod manifest
│   │   └── server/              # actual server directory (user-managed)
│   ├── creative/
│   └── adventure/
└── backups/
    ├── survival/
    │   ├── 2026-06-14_04-00.tar.zst
    │   └── 2026-06-10_04-00.tar.zst
    └── creative/
```

The `server/` subdirectory is where the user places their existing server files. The manager never modifies files outside of `mods/` and the backup directory.

---

## Configuration

### `~/.local/share/msm/config.toml`

```toml
[discord]
token = "BOT_TOKEN_HERE"
guild_id = 123456789
channel_id = 987654321   # channel for join/leave notifications

[web]
port = 8080   # optional, defaults to 8080
```

### `instances/<name>/msm.toml`

```toml
[instance]
name = "Survival"
display_name = "Survival SMP"
minecraft_version = "1.21.1"
loader = "neoforge"
loader_version = "21.1.172"
port = 25565

[server]
# Path to the server directory containing run.sh (NeoForge-generated).
# Can be absolute or relative to this msm.toml file.
path = "./server"

[backup]
enabled = true
schedule = "0 4 * * *"          # cron expression, 4am daily
keep_count = 10                  # delete oldest when exceeded
world_only = false               # true = only world dirs, false = full server dir
```

> NeoForge is started via its generated `run.sh`. JVM args are set through `JAVA_TOOL_OPTIONS` env var if the user wants overrides, but this is not required — the defaults in `run.sh` are usually fine.

### `instances/<name>/mods.lock.toml`

Tracks installed mods. Updated by the mod manager UI, never hand-edited directly.

```toml
[[mods]]
name = "Create"
modrinth_project_id = "LNytGWDc"
modrinth_version_id = "abc123"
filename = "create-1.21.1-6.0.0.jar"
sha512 = "..."

[[mods]]
name = "Apotheosis"
modrinth_project_id = "g96Z4WVZ"
modrinth_version_id = "def456"
filename = "apotheosis-1.21.1-7.3.0.jar"
sha512 = "..."
```

---

## Instance State Machine

Each instance has one of these states:

```
STOPPED → STARTING → RUNNING → STOPPING → STOPPED
                              ↘ CRASHED  → STOPPED
```

- `STARTING`: process spawned, waiting for "Done" in log output
- `RUNNING`: server is accepting connections
- `STOPPING`: `stop` command sent to server stdin; SIGTERM sent as fallback after 30s timeout
- `CRASHED`: process exited unexpectedly (non-zero exit or no graceful shutdown)

Only one instance may be in `STARTING` or `RUNNING` state at a time (port conflict prevention).

**Switch flow:** `RUNNING → STOPPING → STOPPED`, then `STOPPED → STARTING → RUNNING` for the target.

---

## Features

### Instance Management

- Start / Stop / Restart any instance
- Switch: graceful stop of current + start of target (waits for full stop)
- Send arbitrary commands to server stdin via UI or Discord
- Live log streaming to UI (last 1000 lines buffered, tailed live)

### Backups

- Manual backup triggered from UI or Discord command
- Scheduled backup via cron expression per instance
- Backup format: tar archive compressed with zstd
- Retention: delete oldest when `keep_count` exceeded
- Restore: stop instance, extract backup, confirm before overwrite

### Mod Management

- `mods.lock.toml` is the source of truth for installed mods
- "Check for updates": queries Modrinth for each mod, shows available versions
- "Update mod": downloads new JAR, replaces old, updates lock file
- "Update all": batch update with confirmation dialog
- Mod files themselves live in `server/mods/` — manager reads/writes that directory
- No dependency resolution in v1 (user responsible for compatibility)

### Metrics

Parsed from server log output:
- TPS (ticks per second) — from `/forge tps` output or log patterns
- Online player count — from join/leave log events
- RAM usage — from JVM process (`/proc/<pid>/status`)

### Discord Bot

Commands use slash commands via `poise`.

| Command | Description |
|---|---|
| `/status` | Current instance name, state, player count, uptime |
| `/list` | All instances with their current state |
| `/start <instance>` | Start a stopped instance |
| `/stop` | Stop the currently running instance |
| `/restart` | Restart current instance (with 60s in-game warning) |
| `/switch <instance>` | Stop current, start target |
| `/players` | List currently online players |
| `/backup` | Trigger a manual backup of the running instance |
| `/cmd <command>` | Send a command to the server console |

Auto-notifications (to configured channel):
- Server started / stopped / crashed
- Player joined / left
- Backup completed or failed

---

## Frontend

Plain HTML/CSS/JS in three files under `src/ui/`, embedded into the binary via `include_str!`. No framework, no build step.

### Dashboard (main view)

Instance cards in a grid. Each card shows:
- Instance name + Minecraft version
- Status badge (RUNNING / STOPPED / STARTING / CRASHED)
- Player count and names (when running)
- Primary action button (Start / Stop / Switch To) + Details link
- Error message area for failed API calls

Header bar shows a running-instance indicator with animated pulse dot.

### Instance Detail View

Clicking a card navigates to a full-screen detail view with:

- **Logs tab**: streaming log output with level coloring, auto-scroll pinning, command input bar
- **Mods tab**: placeholder (Phase 3)
- **Backups tab**: placeholder (Phase 2)

### UI source files

| File | Purpose |
|---|---|
| `src/ui/index.html` | HTML structure — two views (dashboard, detail) + add-instance modal |
| `src/ui/style.css` | CSS token system + all component styles |
| `src/ui/app.js` | All application logic: SSE handling, rendering, API calls |

---

## Phased Roadmap

### Phase 1 — Core ✓
- [x] axum backend serving plain HTML/CSS/JS via `include_str!`
- [x] Config loading and instance discovery from `~/.local/share/msm/`
- [x] Instance state machine + start/stop/switch via `run.sh`
- [x] Log streaming: stdout → SSE → browser log view
- [x] Dashboard: instance cards with status, player count, quick actions
- [x] "Add Instance" form: generates `msm.toml` from user input
- [x] EULA auto-accept on first start of an instance

### Phase 2 — Backups
- [ ] Manual backup from UI (tar+zstd)
- [ ] Scheduled backup via cron expression
- [ ] Retention policy (delete oldest over `keep_count`)
- [ ] Restore flow: stop instance → extract → confirm overwrite

### Phase 3 — Mods
- [ ] Mod scan: match JARs in `server/mods/` against Modrinth to build initial `mods.lock.toml`
- [ ] Modrinth API client: check latest versions per mod
- [ ] Per-mod and batch update with download + SHA512 verification

### Phase 4 — Discord bot
- [ ] Bot setup, slash command registration
- [ ] Core commands: `/status`, `/start`, `/stop`, `/restart`, `/switch`, `/players`, `/backup`, `/cmd`
- [ ] Auto-notifications: server start/stop/crash, player join/leave

### Phase 5 — Polish
- [ ] RAM and TPS metrics (parsed from logs + `/proc`)
- [ ] Metrics display on instance cards and detail view
- [ ] `JAVA_TOOL_OPTIONS` passthrough for JVM arg overrides
- [ ] Instance creation wizard (NeoForge installer automation)
