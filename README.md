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
- Setup wizard for new instances — pick NeoForge, Forge or Fabric, then choose the Minecraft and loader versions from dropdowns

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

Runs on port `8080` by default. Open `http://localhost:8080` after starting.

To change the port, create `~/.local/share/msm/config.toml`:

```toml
[web]
port = 7331
```

### Dashboard password

Without a password, anyone who can reach the dashboard can control your servers (MSM logs a warning at startup). Set one in `config.toml`:

```toml
[web]
password  = "a long random password"
api_token = "another long random string"   # optional, read-only — for Homepage or scripts
```

The dashboard then shows a login page; sessions last 30 days (or until MSM restarts). The `api_token` is sent as `Authorization: Bearer <token>` and only allows reading (GET requests), so it can't start, stop or change anything.

Requests that change something are always refused when a browser marks them as coming from another website, so other pages you visit can't drive MSM through your browser — with or without a password.

### Environment variables

Every secret and the port can also be set through environment variables, which override `config.toml` (handy for Docker). They are never written back to the file.

| Variable | Overrides |
|----------|-----------|
| `MSM_WEB_PORT` | `[web] port` |
| `MSM_WEB_PASSWORD` | `[web] password` |
| `MSM_API_TOKEN` | `[web] api_token` |
| `MSM_DISCORD_TOKEN` | `[discord] token` |
| `MSM_DISCORD_GUILD_ID` | `[discord] guild_id` |
| `MSM_DISCORD_CHANNEL_ID` | `[discord] channel_id` |

MSM refuses to start if `config.toml` can't be parsed or contains an unknown key in `[web]`, so a typo can't silently disable the password.

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

**Option A — Setup wizard:** open the dashboard and click **Create Server**. Pick a loader and versions; the server is installed to `~/.local/share/msm/servers/<name>/` unless you enter a different directory.

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

When started from a terminal, the dashboard opens in your browser automatically. Logs go to stdout.

## Docker

The image bundles Java 8, 17, 21 and 25 (Temurin) so modpacks for any Minecraft version can run. Java 21 is the default; MSM picks the right one automatically for Modrinth/FTB imports, and you can choose any of them per instance in **Settings**.

A prebuilt image is published to `ghcr.io/lithianfi/minecraft_server_manager` (`latest` tracks master; version tags such as `1.2.0` are published for releases).

```bash
docker compose up -d
```

Then open `http://localhost:8080`. To update: `docker compose pull && docker compose up -d`. To build from source instead, uncomment `build: .` in `docker-compose.yml` and run `docker compose up -d --build`.

> **Maintainers:** the image is built by `.github/workflows/docker.yml` on every push to master and on `v*` tags. GitHub creates the package as private on the first push — make it public under the repository's **Packages → Package settings** so `docker compose pull` works without logging in.

- **Data:** everything lives in the `msm-data` volume, mounted at `/data`. MSM's data directory is `/data/msm` (config, instances, backups). When creating a server with the setup wizard, use a directory under `/data`, e.g. `/data/msm/servers/survival`.
- **Config:** set the [environment variables](#environment-variables) under `environment:` in `docker-compose.yml`, or put a `config.toml` at `/data/msm/config.toml` (`docker compose cp config.toml msm:/data/msm/config.toml`, then restart).
- **Ports:** `8080` is the dashboard; each Minecraft instance's port must also be published. Add a line under `ports:` for every port your instances use.
- **Stopping:** on `docker stop` / `docker compose down`, MSM stops any running server with `stop` so the world is saved. MSM waits up to 3 minutes for the save; the compose file allows 3½ minutes (`stop_grace_period`), and with plain `docker run` pass `--stop-timeout 210`.
- **Bind mount instead of a volume:** the container runs as UID 1000, so the host directory must be writable by it: `mkdir data && sudo chown 1000:1000 data`, then use `./data:/data`.

## Homepage integration

MSM exposes `GET /api/homepage` for [Homepage](https://gethomepage.dev)'s `customapi` widget:

```json
{
  "status": "running",
  "server": "Survival SMP",
  "minecraft_version": "1.21.1",
  "players": 3,
  "tps": 19.98,
  "ram_mb": 6144,
  "cpu_pct": 12.5,
  "uptime_secs": 7260,
  "instances": [{ "name": "Survival SMP", "status": "running" }]
}
```

`status` is `starting`, `running`, `stopping` or `offline`; the other active-server fields are `null` when nothing is running. If you set a [dashboard password](#dashboard-password), give Homepage the read-only `api_token`.

**Docker labels:** the included `docker-compose.yml` already carries Homepage labels. If Homepage has [Docker discovery](https://gethomepage.dev/configs/docker/) enabled and shares a Docker network with MSM, the service appears automatically. Adjust `homepage.href` to the URL you open the dashboard at.

**services.yaml:** or add it manually:

```yaml
- Games:
    - Minecraft:
        icon: minecraft.png
        href: http://your-host:8080
        description: Minecraft Server Manager
        widget:
          type: customapi
          url: http://your-host:8080/api/homepage
          headers:
            Authorization: Bearer your-api-token   # only if you set a [web] password
          mappings:
            - field: status
              label: Status
            - field: players
              label: Players
            - field: tps
              label: TPS
              format: float
            - field: uptime_secs
              label: Uptime
              format: duration
```

To list every instance and its status instead, use `display: dynamic-list` with `mappings: { items: instances, name: name, label: status }`.
