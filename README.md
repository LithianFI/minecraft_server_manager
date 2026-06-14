# Minecraft Server Manager

A self-hosted Minecraft server manager built in Rust. Runs a local web dashboard for managing NeoForge server instances, with an optional Discord bot for remote control.

## Features

### Instance Management
- Run multiple NeoForge server instances (one active at a time, switch between them)
- Auto-discovery: drop a server folder into `~/.local/share/msm/instances/` and it appears automatically
- Per-instance Java version override (useful when different modpacks need different JVM versions)
- NeoForge / Minecraft version updater — runs the installer in-place with live progress output

### Web Dashboard
- Real-time console log with level filtering (All / Info / Warn / Error) and free-text search
- Live player list, TPS, and RAM metrics
- Server properties editor (preserves comments and ordering in `server.properties`)
- Setup wizard for new instances (downloads and runs NeoForge installer)

### Backups
- zstd-compressed tar archives
- Configurable retention (keep N most recent backups)
- World-only or full-server backup
- Restore from any listed backup

### Mods
- Scans installed mods and checks Modrinth for updates
- One-click update for individual mods or all at once

### Whitelist & Bans
- Global whitelist synced across all server instances
- Player and IP ban management synced across all instances
- Mojang UUID lookup by username

### Crash Recovery & Scheduled Restarts
- Auto-restart on crash with configurable max attempts and delay
- Cron-based scheduled restarts with in-game countdown warnings (5 min / 1 min / 30 s / 10 s)

### Discord Bot
| Command | Description |
|---------|-------------|
| `/status` | Show the currently running server and uptime |
| `/list` | List all instances and their status |
| `/start <instance>` | Start a stopped instance |
| `/stop` | Stop the running instance |
| `/restart` | Restart with a 30 s in-game warning |
| `/switch <instance>` | Stop current and start another instance |
| `/players` | List online players |
| `/backup` | Trigger a backup of the running instance |
| `/cmd <command>` | Send a command to the server console |
| `/ip` | Show the server's current public IP address |

The bot also posts notifications to a configured channel when servers start, stop, crash, players join/leave, or backups complete.

## Requirements

- Rust (stable, edition 2024)
- Java — whichever version your modpack requires (configurable per instance)
- A NeoForge server installation (the setup wizard can create one)

## Installation

```bash
git clone https://github.com/LithianFI/minecraft_server_manager.git
cd minecraft_server_manager
cargo build --release
```

The binary is at `target/release/minecraft_server_manager`.

## Configuration

All data lives in `~/.local/share/msm/`.

### Web dashboard

Runs on port `7331` by default. Open `http://localhost:7331` after starting.

To change the port, create `~/.local/share/msm/config.toml`:

```toml
[web]
port = 8080
```

### Discord bot

Add a `[discord]` section to `config.toml`:

```toml
[discord]
token      = "YOUR_BOT_TOKEN"
guild_id   = 123456789012345678
channel_id = 123456789012345678
```

**Getting these values:**
1. Create an application at [discord.com/developers/applications](https://discord.com/developers/applications) → Bot → copy the token
2. Enable Developer Mode in Discord (Settings → Advanced) then right-click your server → Copy Server ID (`guild_id`) and right-click your notification channel → Copy Channel ID (`channel_id`)
3. Invite the bot via OAuth2 → URL Generator with scopes `bot` + `applications.commands` and at minimum the **Send Messages** permission

### Adding a server instance

**Option A — Setup wizard:** open the dashboard and click **New Instance**. The wizard downloads NeoForge and runs the installer.

**Option B — Auto-discovery:** copy an existing NeoForge server folder into `~/.local/share/msm/instances/`. The manager will detect it on the next scan (within 30 seconds) and generate an `msm.toml` from `run.sh` and `server.properties` automatically.

**Option C — Manual config:** create `~/.local/share/msm/instances/<name>/msm.toml`:

```toml
[instance]
name              = "my-server"
display_name      = "My Server"
minecraft_version = "1.21.1"
loader            = "neoforge"
loader_version    = "21.1.172"
port              = 25565

[server]
path = "/path/to/server"

# Optional: use a specific Java installation
# java_path = "/usr/lib/jvm/java-21-openjdk/bin/java"

# Optional: extra JVM flags
# java_opts = "-Xmx8G -Xms2G"

[backup]
enabled    = true
keep_count = 10
world_only = false

[restart]
auto_restart  = true
max_attempts  = 3
delay_secs    = 10
warning_secs  = 300
# schedule    = "0 4 * * *"   # restart daily at 04:00 (cron, 5 or 6 fields)
```

## Running

```bash
./target/release/minecraft_server_manager
```

The dashboard opens in your browser automatically. Logs go to stdout.
