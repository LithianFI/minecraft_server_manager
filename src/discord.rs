use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use poise::serenity_prelude as serenity;

use crate::{
    backup,
    config::{DiscordConfig, DiscordNotifyConfig},
    instance,
    state::{AppState, InstanceStatus, WsEvent},
};

// ── Framework types ───────────────────────────────────────────────────────────

struct BotData {
    state: Arc<AppState>,
}

impl std::fmt::Debug for BotData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotData").finish_non_exhaustive()
    }
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, BotData, Error>;

// ── Shared helpers ────────────────────────────────────────────────────────────

async fn running_instance(state: &AppState) -> Option<(String, String)> {
    let instances = state.instances.read().await;
    instances
        .values()
        .find(|i| matches!(i.status, InstanceStatus::Running | InstanceStatus::Starting))
        .map(|i| {
            let display = i.config.instance.display_name.clone()
                .unwrap_or_else(|| i.config.instance.name.clone());
            (i.id.clone(), display)
        })
}

fn status_icon(s: &InstanceStatus) -> &'static str {
    match s {
        InstanceStatus::Running  => "🟢",
        InstanceStatus::Starting => "🟡",
        InstanceStatus::Stopping => "🟠",
        InstanceStatus::Stopped  => "⚫",
        InstanceStatus::Crashed  => "🔴",
    }
}

fn status_label(s: &InstanceStatus) -> &'static str {
    match s {
        InstanceStatus::Running  => "RUNNING",
        InstanceStatus::Starting => "STARTING",
        InstanceStatus::Stopping => "STOPPING",
        InstanceStatus::Stopped  => "STOPPED",
        InstanceStatus::Crashed  => "CRASHED",
    }
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

async fn autocomplete_instance<'a>(ctx: Context<'a>, partial: &'a str) -> Vec<String> {
    let instances = ctx.data().state.instances.read().await;
    let partial_lower = partial.to_lowercase();
    let mut matches: Vec<String> = instances
        .values()
        .filter(|i| {
            let name = i.config.instance.display_name.as_deref()
                .unwrap_or(&i.config.instance.name)
                .to_lowercase();
            i.id.contains(&partial_lower) || name.contains(&partial_lower)
        })
        .map(|i| i.id.clone())
        .collect();
    matches.sort();
    matches
}

// ── Player confirmation helper ────────────────────────────────────────────────

/// Returns the online players for the running instance (empty if none running).
async fn online_players(state: &AppState) -> Vec<String> {
    let instances = state.instances.read().await;
    instances
        .values()
        .find(|i| matches!(i.status, InstanceStatus::Running | InstanceStatus::Starting))
        .map(|i| i.players.iter().cloned().collect())
        .unwrap_or_default()
}

/// Shows a Danger/Cancel button prompt when players are online.
/// Returns `true` if the user confirmed (or there were no players), `false` if cancelled/timed out.
async fn player_confirm(ctx: Context<'_>, action: &str, players: &[String]) -> Result<bool, Error> {
    if players.is_empty() {
        return Ok(true);
    }

    let yes_id = format!("msm_yes_{}", ctx.id());
    let no_id  = format!("msm_no_{}", ctx.id());

    let names = players.iter().map(|p| format!("`{}`", p)).collect::<Vec<_>>().join(", ");
    let prompt = format!(
        "⚠️ {} player(s) currently online: {}\n{} anyway?",
        players.len(), names, action
    );

    let reply = ctx.send(
        poise::CreateReply::default()
            .content(&prompt)
            .components(vec![serenity::CreateActionRow::Buttons(vec![
                serenity::CreateButton::new(&yes_id)
                    .label(action)
                    .style(serenity::ButtonStyle::Danger),
                serenity::CreateButton::new(&no_id)
                    .label("Cancel")
                    .style(serenity::ButtonStyle::Secondary),
            ])])
    ).await?;

    let msg = reply.message().await?;

    let interaction = serenity::collector::ComponentInteractionCollector::new(ctx.serenity_context())
        .message_id(msg.id)
        .author_id(ctx.author().id)
        .timeout(Duration::from_secs(30))
        .next()
        .await;

    let confirmed = interaction.as_ref().map(|i| i.data.custom_id == yes_id).unwrap_or(false);

    if let Some(press) = &interaction {
        press.create_response(ctx.serenity_context(), serenity::CreateInteractionResponse::Acknowledge).await.ok();
    }

    let result_text = if confirmed {
        prompt
    } else if interaction.is_none() {
        "⏱️ Confirmation timed out — no action taken.".to_string()
    } else {
        "❌ Cancelled.".to_string()
    };
    reply.edit(ctx, poise::CreateReply::default().content(result_text).components(vec![])).await?;

    Ok(confirmed)
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// Show the currently running server
#[poise::command(slash_command)]
async fn status(ctx: Context<'_>) -> Result<(), Error> {
    let instances = ctx.data().state.instances.read().await;
    let running = instances.values()
        .find(|i| matches!(i.status, InstanceStatus::Running | InstanceStatus::Starting));

    let msg = if let Some(inst) = running {
        let display = inst.config.instance.display_name.as_deref()
            .unwrap_or(&inst.config.instance.name);
        let uptime = inst.started_at.map(|t| {
            let s = (chrono::Utc::now() - t).num_seconds().max(0);
            format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
        }).unwrap_or_else(|| "unknown".into());
        format!("🟢 **{}** — {} player(s) online — up {}", display, inst.players.len(), uptime)
    } else {
        "⚫ No server is currently running.".to_string()
    };

    ctx.say(msg).await?;
    Ok(())
}

/// List all instances and their status
#[poise::command(slash_command)]
async fn list(ctx: Context<'_>) -> Result<(), Error> {
    let instances = ctx.data().state.instances.read().await;
    if instances.is_empty() {
        ctx.say("No instances configured.").await?;
        return Ok(());
    }
    let mut lines: Vec<String> = instances.values()
        .map(|i| {
            let display = i.config.instance.display_name.as_deref()
                .unwrap_or(&i.config.instance.name);
            format!("{} **{}** — {}", status_icon(&i.status), display, status_label(&i.status))
        })
        .collect();
    lines.sort();
    ctx.say(lines.join("\n")).await?;
    Ok(())
}

/// Start a stopped instance
#[poise::command(slash_command)]
async fn start(
    ctx: Context<'_>,
    #[description = "Instance to start"]
    #[autocomplete = "autocomplete_instance"]
    instance_id: String,
) -> Result<(), Error> {
    let state = ctx.data().state.clone();
    ctx.defer().await?;
    match instance::start_instance(state, &instance_id).await {
        Ok(_)  => ctx.say(format!("▶ Starting **{}**…", instance_id)).await?,
        Err(e) => ctx.say(format!("❌ {}", e)).await?,
    };
    Ok(())
}

/// Stop the currently running instance
#[poise::command(slash_command)]
async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let state = ctx.data().state.clone();
    let Some((id, display)) = running_instance(&state).await else {
        ctx.say("⚫ No server is running.").await?;
        return Ok(());
    };
    let players = online_players(&state).await;
    if !player_confirm(ctx, "Stop", &players).await? {
        return Ok(());
    }
    match instance::stop_instance(state, &id).await {
        Ok(_)  => ctx.say(format!("⏹ Stopping **{}**…", display)).await?,
        Err(e) => ctx.say(format!("❌ {}", e)).await?,
    };
    Ok(())
}

/// Restart the running instance (sends 30s in-game warning)
#[poise::command(slash_command)]
async fn restart(ctx: Context<'_>) -> Result<(), Error> {
    let state = ctx.data().state.clone();
    let Some((id, display)) = running_instance(&state).await else {
        ctx.say("⚫ No server is running.").await?;
        return Ok(());
    };

    ctx.defer().await?;

    let _ = instance::send_command(
        state.clone(), &id,
        "say [MSM] Server restarting in 30 seconds!".to_string(),
    ).await;

    tokio::time::sleep(Duration::from_secs(30)).await;

    if let Err(e) = instance::stop_instance(state.clone(), &id).await {
        ctx.say(format!("❌ Stop failed: {}", e)).await?;
        return Ok(());
    }

    // Poll until stopped (max 60s)
    let _ = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let done = {
                let instances = state.instances.read().await;
                instances.get(&id)
                    .map(|i| matches!(i.status, InstanceStatus::Stopped | InstanceStatus::Crashed))
                    .unwrap_or(true)
            };
            if done { break; }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }).await;

    match instance::start_instance(state, &id).await {
        Ok(_)  => ctx.say(format!("🔄 **{}** is restarting.", display)).await?,
        Err(e) => ctx.say(format!("❌ Start failed after stop: {}", e)).await?,
    };
    Ok(())
}

/// Switch to a different instance (stops current, starts target)
#[poise::command(slash_command)]
async fn switch(
    ctx: Context<'_>,
    #[description = "Instance to switch to"]
    #[autocomplete = "autocomplete_instance"]
    instance_id: String,
) -> Result<(), Error> {
    let state = ctx.data().state.clone();
    let players = online_players(&state).await;
    if !player_confirm(ctx, "Switch", &players).await? {
        return Ok(());
    }
    ctx.defer().await?;
    match instance::switch_instance(state, &instance_id).await {
        Ok(_)  => ctx.say(format!("🔀 Switched to **{}**.", instance_id)).await?,
        Err(e) => ctx.say(format!("❌ {}", e)).await?,
    };
    Ok(())
}

/// List online players
#[poise::command(slash_command)]
async fn players(ctx: Context<'_>) -> Result<(), Error> {
    let instances = ctx.data().state.instances.read().await;
    let running = instances.values()
        .find(|i| matches!(i.status, InstanceStatus::Running | InstanceStatus::Starting));

    let msg = if let Some(inst) = running {
        let display = inst.config.instance.display_name.as_deref()
            .unwrap_or(&inst.config.instance.name);
        if inst.players.is_empty() {
            format!("No players online on **{}**.", display)
        } else {
            let list = inst.players.iter()
                .map(|p| format!("`{}`", p))
                .collect::<Vec<_>>()
                .join(", ");
            format!("**{}** — {} player(s): {}", display, inst.players.len(), list)
        }
    } else {
        "⚫ No server is running.".to_string()
    };

    ctx.say(msg).await?;
    Ok(())
}

/// Trigger a backup of the running instance
#[poise::command(slash_command)]
async fn backup_cmd(ctx: Context<'_>) -> Result<(), Error> {
    let state = ctx.data().state.clone();
    let Some((id, display)) = running_instance(&state).await else {
        ctx.say("⚫ No server is running.").await?;
        return Ok(());
    };
    ctx.say(format!("💾 Backup of **{}** started…", display)).await?;
    tokio::spawn(backup::trigger_backup(state, id));
    Ok(())
}

/// List all installed mods with versions and Modrinth links
#[poise::command(slash_command)]
async fn mods(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;

    let state = ctx.data().state.clone();

    let (instance_dir, display_name, mc_version, loader, loader_version, modpack_project_id) = {
        let instances = state.instances.read().await;
        // Prefer the running instance; fall back to the first available
        let inst = instances
            .values()
            .find(|i| matches!(i.status, InstanceStatus::Running | InstanceStatus::Starting))
            .or_else(|| instances.values().next());
        match inst {
            Some(i) => (
                i.instance_dir.clone(),
                i.config.instance.display_name.clone()
                    .unwrap_or_else(|| i.config.instance.name.clone()),
                i.config.instance.minecraft_version.clone(),
                i.config.instance.loader.clone().unwrap_or_else(|| "unknown".to_string()),
                i.config.instance.loader_version.clone(),
                i.config.instance.modrinth_project_id.clone(),
            ),
            None => {
                ctx.say("No instances configured.").await?;
                return Ok(());
            }
        }
    };

    let mod_lock = crate::mod_mgr::read_lock(&instance_dir);
    let dp_lock  = crate::datapack_mgr::read_lock(&instance_dir);

    if mod_lock.mods.is_empty() && dp_lock.datapacks.is_empty() {
        ctx.say("No mods or datapacks found. Run a scan from the dashboard first.").await?;
        return Ok(());
    }

    let loader_display = match loader_version {
        Some(ref v) => format!("{} {}", loader, v),
        None => loader.clone(),
    };

    let modpack_line = modpack_project_id.as_deref()
        .map(|id| format!("Modpack: https://modrinth.com/modpack/{}\n", id))
        .unwrap_or_default();

    let mut lines = format!(
        "{} — Minecraft {} | {} — {} mod{}, {} datapack{}\n{}\n",
        display_name,
        mc_version,
        loader_display,
        mod_lock.mods.len(),
        if mod_lock.mods.len() == 1 { "" } else { "s" },
        dp_lock.datapacks.len(),
        if dp_lock.datapacks.len() == 1 { "" } else { "s" },
        modpack_line,
    );

    if !mod_lock.mods.is_empty() {
        lines.push_str(&format!("── Mods ({}) ──\n", mod_lock.mods.len()));
        for m in &mod_lock.mods {
            lines.push_str(&format!(
                "{} v{} — https://modrinth.com/mod/{}\n",
                m.name, m.version_number, m.modrinth_project_id
            ));
        }
    }

    if !dp_lock.datapacks.is_empty() {
        if !mod_lock.mods.is_empty() {
            lines.push('\n');
        }
        lines.push_str(&format!("── Datapacks ({}) ──\n", dp_lock.datapacks.len()));
        for d in &dp_lock.datapacks {
            lines.push_str(&format!(
                "{} v{} — https://modrinth.com/datapack/{}\n",
                d.name, d.version_number, d.modrinth_project_id
            ));
        }
    }

    let modpack_suffix = modpack_project_id.as_deref()
        .map(|id| format!(" • <https://modrinth.com/modpack/{}>", id))
        .unwrap_or_default();

    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "📦 **{}** — Minecraft {} | {} — {} mod{}, {} datapack{}{}",
                display_name,
                mc_version,
                loader_display,
                mod_lock.mods.len(),
                if mod_lock.mods.len() == 1 { "" } else { "s" },
                dp_lock.datapacks.len(),
                if dp_lock.datapacks.len() == 1 { "" } else { "s" },
                modpack_suffix,
            ))
            .attachment(serenity::CreateAttachment::bytes(lines.into_bytes(), "mods.txt")),
    )
    .await?;

    Ok(())
}

/// Show the public IP address of this server
#[poise::command(slash_command)]
async fn ip(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let ip = reqwest::get("https://api.ipify.org")
        .await?
        .text()
        .await?;
    ctx.say(format!("🌐 Public IP: `{}`", ip.trim())).await?;
    Ok(())
}

/// Send a command to the server console
#[poise::command(slash_command)]
async fn cmd(
    ctx: Context<'_>,
    #[description = "Command to send to the server console"]
    command: String,
) -> Result<(), Error> {
    let state = ctx.data().state.clone();
    let Some((id, _)) = running_instance(&state).await else {
        ctx.say("⚫ No server is running.").await?;
        return Ok(());
    };
    match instance::send_command(state, &id, command.clone()).await {
        Ok(_)  => ctx.say(format!("`> {}`", command)).await?,
        Err(e) => ctx.say(format!("❌ {}", e)).await?,
    };
    Ok(())
}

async fn autocomplete_player<'a>(ctx: Context<'a>, partial: &'a str) -> Vec<String> {
    let instances = ctx.data().state.instances.read().await;
    let partial_lower = partial.to_lowercase();
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for inst in instances.values() {
        for name in inst.last_deaths.keys() {
            names.insert(name.clone());
        }
        for name in &inst.players {
            names.insert(name.clone());
        }
    }
    let mut matches: Vec<String> = names
        .into_iter()
        .filter(|n| n.to_lowercase().contains(&partial_lower))
        .collect();
    matches.sort();
    matches
}

/// Show where a player last died
#[poise::command(slash_command)]
async fn lastdeath(
    ctx: Context<'_>,
    #[description = "Player name"]
    #[autocomplete = "autocomplete_player"]
    player: String,
) -> Result<(), Error> {
    let instances = ctx.data().state.instances.read().await;
    let death = instances
        .values()
        .filter_map(|inst| inst.last_deaths.get(&player))
        .max_by_key(|d| d.timestamp);

    let msg = match death {
        None => format!("No death recorded for **{}** since the bot started.", player),
        Some(d) => {
            let time = chrono::DateTime::<chrono::Utc>::from_timestamp_secs(d.timestamp)
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "unknown time".to_string());
            match (d.x, d.y, d.z, &d.dimension) {
                (Some(x), Some(y), Some(z), Some(dim)) => {
                    let dim_short = dim.trim_start_matches("minecraft:");
                    format!("💀 **{}** last died at `{}, {}, {}` in **{}** ({})", player, x, y, z, dim_short, time)
                }
                _ => format!("💀 **{}** last died at unknown coordinates ({})", player, time),
            }
        }
    };

    ctx.say(msg).await?;
    Ok(())
}

// ── Notification task ─────────────────────────────────────────────────────────

async fn notify_task(
    http: Arc<serenity::Http>,
    channel_id: u64,
    mut rx: broadcast::Receiver<WsEvent>,
    notify: Arc<tokio::sync::RwLock<DiscordNotifyConfig>>,
) {
    let channel = serenity::ChannelId::new(channel_id);
    loop {
        match rx.recv().await {
            Ok(event) => {
                let cfg = notify.read().await.clone();
                let msg: Option<String> = match &event {
                    WsEvent::StateChanged { instance_id, status } => match status {
                        InstanceStatus::Running if cfg.server_started =>
                            Some(format!("🟢 **{}** is now running.", instance_id)),
                        InstanceStatus::Stopped if cfg.server_stopped =>
                            Some(format!("⚫ **{}** has stopped.", instance_id)),
                        InstanceStatus::Crashed if cfg.server_crashed =>
                            Some(format!("🔴 **{}** has crashed!", instance_id)),
                        _ => None,
                    },
                    WsEvent::BackupDone { instance_id, filename, size_bytes } if cfg.backup_done => {
                        let mb = *size_bytes as f64 / 1_048_576.0;
                        Some(format!("💾 Backup of **{}** done — `{}` ({:.1} MB)", instance_id, filename, mb))
                    }
                    WsEvent::BackupFailed { instance_id, error } if cfg.backup_failed =>
                        Some(format!("⚠️ Backup of **{}** failed: {}", instance_id, error)),
                    WsEvent::HealthAlert { instance_id, kind, message } if cfg.health_alerts => {
                        let icon = if kind == "tps" { "⚠️" } else { "🔥" };
                        Some(format!("{} **{}** health alert: {}", icon, instance_id, message))
                    }
                    _ => None,
                };
                if let Some(text) = msg {
                    if let Err(e) = channel.say(&http, text).await {
                        tracing::warn!("Discord notification failed: {}", e);
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("Discord notifier lagged {} events", n);
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn start_bot(state: Arc<AppState>, config: DiscordConfig) {
    tokio::spawn(run_bot(state, config));
}

async fn run_bot(state: Arc<AppState>, config: DiscordConfig) {
    let token = config.token.clone();
    let guild_id = config.guild_id;
    let channel_id = config.channel_id;

    // Subscribe before connecting so we don't miss early events
    let notify_rx = state.log_tx.subscribe();
    let http = Arc::new(serenity::Http::new(&token));
    tokio::spawn(notify_task(http, channel_id, notify_rx, state.discord_notify.clone()));

    let state_data = state.clone();
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                status(),
                list(),
                start(),
                stop(),
                restart(),
                switch(),
                players(),
                lastdeath(),
                backup_cmd(),
                cmd(),
                ip(),
                mods(),
            ],
            on_error: |err| {
                Box::pin(async move {
                    tracing::error!("Discord command error: {:?}", err);
                })
            },
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            let state = state_data.clone();
            Box::pin(async move {
                poise::builtins::register_in_guild(
                    ctx,
                    &framework.options().commands,
                    serenity::GuildId::new(guild_id),
                )
                .await?;
                tracing::info!("Discord bot ready, slash commands registered in guild {}", guild_id);
                Ok(BotData { state })
            })
        })
        .build();

    let mut client = match serenity::ClientBuilder::new(&token, serenity::GatewayIntents::non_privileged())
        .framework(framework)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to create Discord client: {}", e);
            return;
        }
    };

    if let Err(e) = client.start().await {
        tracing::error!("Discord client error: {}", e);
    }
}
