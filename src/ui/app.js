// ─── State ───────────────────────────────────────────────────────────────────
const instances  = new Map()   // id → InstanceInfo
const logs       = new Map()   // id → [{line, timestamp}]
const backups    = new Map()   // id → [BackupInfo]
const modsData       = new Map()   // id → { mods: [], updates: null }
const datapacksData  = new Map()   // id → { datapacks: [], updates: null }
let   detailId   = null
let   logPinned  = true
let   logFilter  = 'all'
let   logSearch  = ''
let   logRegex   = false

// ─── Command history & macros ─────────────────────────────────────────────────
let cmdHistory    = JSON.parse(localStorage.getItem('cmd_history') || '[]')
let cmdHistoryPos = -1
let cmdMacros     = JSON.parse(localStorage.getItem('cmd_macros')  || '[]')
let macrosOpen    = false

// ─── Toasts ───────────────────────────────────────────────────────────────────
let _toastId = 0

// ─── Boot ─────────────────────────────────────────────────────────────────────
window.addEventListener('DOMContentLoaded', setupSSE)

// ─── SSE ─────────────────────────────────────────────────────────────────────
function setupSSE() {
  const es = new EventSource('/events')
  es.onmessage = (e) => {
    try { handleEvent(JSON.parse(e.data)) } catch { /* ignore malformed */ }
  }
  // EventSource reconnects automatically on error
}

function handleEvent(ev) {
  switch (ev.type) {
    case 'init':
      instances.clear()
      ev.instances.forEach(i => instances.set(i.id, i))
      renderDashboard()
      if (detailId) refreshDetail(instances.get(detailId))
      break

    case 'log_history':
      logs.set(ev.instance_id, ev.lines)
      if (detailId === ev.instance_id) renderLogs()
      break

    case 'log_line': {
      const buf = logs.get(ev.instance_id) ?? []
      if (buf.length >= 1000) buf.shift()
      buf.push({ line: ev.line, timestamp: ev.timestamp })
      logs.set(ev.instance_id, buf)
      if (detailId === ev.instance_id) appendLogLine(ev.line, ev.timestamp)
      break
    }

    case 'state_changed': {
      const inst = instances.get(ev.instance_id)
      if (!inst) break
      const prevStatus = inst.status
      inst.status = ev.status
      if (ev.status !== 'running' && ev.status !== 'starting') {
        inst.ram_mb  = null
        inst.tps     = null
        inst.cpu_pct = null
      }
      updateCard(inst)
      if (detailId === ev.instance_id) {
        refreshDetail(inst)
        refreshMetricsBar(inst)
      }
      if (ev.status === 'crashed') {
        showToast(inst.display_name, 'Server crashed', 'error')
      } else if (ev.status === 'running' && prevStatus === 'starting') {
        showToast(inst.display_name, 'Server started', 'success')
      }
      break
    }

    case 'player_joined': {
      const inst = instances.get(ev.instance_id)
      if (!inst || inst.players.includes(ev.player)) break
      inst.players.push(ev.player)
      updateCard(inst)
      if (detailId === ev.instance_id) refreshPlayerBar(inst)
      else showToast(ev.player + ' joined', inst.display_name, 'info', 4000)
      break
    }

    case 'player_left': {
      const inst = instances.get(ev.instance_id)
      if (!inst) break
      inst.players = inst.players.filter(p => p !== ev.player)
      updateCard(inst)
      if (detailId === ev.instance_id) refreshPlayerBar(inst)
      else showToast(ev.player + ' left', inst.display_name, 'info', 4000)
      break
    }

    case 'backup_done': {
      setBackupMsg(`Backup created (${fmtSize(ev.size_bytes)})`, 'success')
      document.getElementById('btn-create-backup').disabled = false
      if (ev.instance_id === detailId) loadBackups(detailId)
      showToast(instances.get(ev.instance_id)?.display_name ?? ev.instance_id, `Backup created · ${fmtSize(ev.size_bytes)}`, 'success')
      break
    }

    case 'backup_failed': {
      setBackupMsg(`Backup failed: ${ev.error}`, 'error')
      document.getElementById('btn-create-backup').disabled = false
      showToast(instances.get(ev.instance_id)?.display_name ?? ev.instance_id, `Backup failed: ${ev.error}`, 'error')
      break
    }

    case 'metrics': {
      const inst = instances.get(ev.instance_id)
      if (!inst) break
      inst.ram_mb = ev.ram_mb
      if (ev.tps != null) inst.tps = ev.tps
      if (ev.cpu_pct != null) inst.cpu_pct = ev.cpu_pct
      updateCard(inst)
      if (detailId === ev.instance_id) refreshMetricsBar(inst)
      break
    }

    case 'auto_restarting': {
      const inst = instances.get(ev.instance_id)
      if (inst) updateCard(inst)
      if (detailId === ev.instance_id) {
        const ts = Math.floor(Date.now() / 1000)
        appendLogLine(`[MSM] Auto-restarting… (attempt ${ev.attempt}/${ev.max_attempts})`, ts)
      }
      showToast(inst?.display_name ?? ev.instance_id, `Auto-restarting… (${ev.attempt}/${ev.max_attempts})`, 'warning')
      break
    }

    case 'update_log':
      if (ev.instance_id === detailId) appendUpdateLog(ev.message)
      break
    case 'update_done': {
      const inst = instances.get(ev.instance_id)
      if (inst) {
        inst.minecraft_version = ev.minecraft_version
        updateCard(inst)
        if (detailId === ev.instance_id) refreshDetail(inst)
      }
      onUpdateDone(ev.instance_id)
      break
    }
    case 'update_failed':
      onUpdateFailed(ev.instance_id, ev.error)
      break

    case 'instance_added': {
      instances.set(ev.instance.id, ev.instance)
      logs.set(ev.instance.id, [])
      renderDashboard()
      break
    }

    case 'setup_log':
      appendSetupLog(ev.message)
      break
    case 'setup_done':
      onSetupDone(ev.server_path)
      break
    case 'setup_failed':
      onSetupFailed(ev.error)
      break

    case 'modpack_log':
      if (!document.getElementById('ftb-step-2').classList.contains('hidden')) {
        appendFtbLog(ev.message)
      } else {
        appendImportLog(ev.message)
      }
      break
    case 'modpack_done':
      if (!document.getElementById('ftb-step-2').classList.contains('hidden')) {
        onFtbDone()
      } else {
        onModpackDone()
      }
      break
    case 'modpack_failed':
      if (!document.getElementById('ftb-step-2').classList.contains('hidden')) {
        onFtbFailed(ev.error)
      } else {
        onModpackFailed(ev.error)
      }
      break
  }
}

// ─── Navigation ───────────────────────────────────────────────────────────────
function showDetail(id) {
  const inst = instances.get(id)
  if (!inst) return
  detailId = id
  logPinned = true
  logFilter = 'all'
  logSearch = ''
  logRegex  = false
  const searchEl = document.getElementById('log-search')
  if (searchEl) searchEl.value = ''
  const regexBtn = document.getElementById('btn-regex')
  if (regexBtn) regexBtn.classList.remove('active')
  document.querySelectorAll('.log-filter-btn').forEach(b => b.classList.toggle('active', b.dataset.level === 'all'))
  document.getElementById('view-dashboard').classList.add('hidden')
  document.getElementById('view-detail').classList.remove('hidden')
  refreshDetail(inst)
  switchTab('logs')
  renderLogs()
  loadBackups(id)
  loadMods(id)
  loadDatapacks(id)
}

function showDashboard() {
  detailId = null
  document.getElementById('view-detail').classList.add('hidden')
  document.getElementById('view-dashboard').classList.remove('hidden')
}

// ─── Dashboard ────────────────────────────────────────────────────────────────
function renderDashboard() {
  const arr     = [...instances.values()].sort((a, b) => a.display_name.localeCompare(b.display_name))
  const running = arr.find(i => i.status === 'running' || i.status === 'starting')
  const grid    = document.getElementById('instances-grid')
  const empty   = document.getElementById('empty-state')
  const badge   = document.getElementById('running-badge')

  // Running instance indicator in header
  if (running) {
    document.getElementById('running-name').textContent = running.display_name
    badge.classList.remove('hidden')
  } else {
    badge.classList.add('hidden')
  }

  if (arr.length === 0) {
    grid.innerHTML = ''
    empty.classList.remove('hidden')
    renderDashboardPlayerPanel()
    return
  }
  empty.classList.add('hidden')
  grid.innerHTML = arr.map(i => cardHTML(i, running)).join('')
  renderDashboardPlayerPanel()
}

// Targeted card update — avoids re-rendering the full grid on every event
function updateCard(inst) {
  const el = document.getElementById('card-' + inst.id)
  if (!el) { renderDashboard(); return }

  const running = [...instances.values()].find(i => (i.status === 'running' || i.status === 'starting') && i.id !== inst.id)
    ?? (inst.status === 'running' || inst.status === 'starting' ? inst : undefined)

  el.className = 'card card-' + inst.status
  el.querySelector('.badge').className = 'badge badge-' + inst.status
  el.querySelector('.badge').textContent = statusLabel(inst.status)
  el.querySelector('.card-mid').innerHTML = cardMidHTML(inst)
  el.querySelector('.card-actions').innerHTML = cardActionsHTML(inst, running)

  // Update header running badge
  const runningBadge = document.getElementById('running-badge')
  const anyRunning = [...instances.values()].find(i => i.status === 'running' || i.status === 'starting')
  if (anyRunning) {
    document.getElementById('running-name').textContent = anyRunning.display_name
    runningBadge.classList.remove('hidden')
  } else {
    runningBadge.classList.add('hidden')
  }

  renderDashboardPlayerPanel()
}

function cardHTML(inst, running) {
  return `<div id="card-${inst.id}" class="card card-${inst.status}" onclick="showDetail('${inst.id}')">
    <div class="card-top">
      <div class="card-name-row">
        <span class="card-name">${esc(inst.display_name)}</span>
        <span class="badge badge-${inst.status}">${statusLabel(inst.status)}</span>
      </div>
      <span class="card-ver">${esc(inst.minecraft_version)}</span>
    </div>
    <div class="card-mid">${cardMidHTML(inst)}</div>
    <div id="card-error-${inst.id}" class="card-error hidden"></div>
    <div class="card-actions" onclick="event.stopPropagation()">${cardActionsHTML(inst, running)}</div>
  </div>`
}

function cardMidHTML(inst) {
  const running = inst.status === 'running' || inst.status === 'starting'
  if (!running) return `<div class="card-port">:${inst.port}</div>`

  const count = inst.players.length
  const playerStr = count === 0
    ? `<span class="card-player-count dim">No players</span>`
    : `<span class="card-player-count">${count} player${count !== 1 ? 's' : ''}</span>`

  let metricsStr = ''
  if (inst.ram_mb != null) {
    const ram = inst.ram_mb >= 1024 ? `${(inst.ram_mb / 1024).toFixed(1)} GB` : `${inst.ram_mb} MB`
    const tps = inst.tps != null ? ` · <span class="card-tps ${tpsClass(inst.tps)}">${inst.tps.toFixed(1)} TPS</span>` : ''
    metricsStr = `<span class="card-metrics">${ram} RAM${tps}</span>`
  }

  const sep = metricsStr ? `<span class="card-mid-sep">·</span>` : ''
  return `<div class="card-stats-row">${playerStr}${sep}${metricsStr}</div>`
}

function tpsClass(tps) {
  if (tps >= 18) return 'tps-good'
  if (tps >= 14) return 'tps-warn'
  return 'tps-bad'
}

function cardActionsHTML(inst, running) {
  const isRunning = inst.status === 'running' || inst.status === 'starting'
  const isBusy    = inst.status === 'starting' || inst.status === 'stopping'
  const canStart  = inst.status === 'stopped' || inst.status === 'crashed'
  const canSwitch = canStart && running && running.id !== inst.id

  let primary = ''
  if (isRunning) {
    primary = `<button class="btn-stop" onclick="doStop('${inst.id}')" ${isBusy ? 'disabled' : ''}>Stop</button>`
  } else if (canSwitch) {
    primary = `<button class="btn-switch" onclick="doSwitch('${inst.id}')">Switch To</button>`
  } else if (canStart) {
    primary = `<button class="btn-start" onclick="doStart('${inst.id}')" ${running ? 'disabled' : ''}>Start</button>`
  }
  return primary + `<button class="btn-detail" onclick="showDetail('${inst.id}')">Details →</button>`
}

// ─── Detail view ──────────────────────────────────────────────────────────────
function refreshDetail(inst) {
  if (!inst) return
  document.getElementById('detail-name').textContent    = inst.display_name
  document.getElementById('detail-version').textContent = inst.minecraft_version

  const badge = document.getElementById('detail-badge')
  badge.className   = 'badge badge-' + inst.status
  badge.textContent = statusLabel(inst.status)

  const isRunning = inst.status === 'running' || inst.status === 'starting'
  const isBusy    = inst.status === 'starting' || inst.status === 'stopping'

  const btnStart = document.getElementById('btn-detail-start')
  const btnStop  = document.getElementById('btn-detail-stop')

  btnStart.classList.toggle('hidden',  isRunning)
  btnStop.classList.toggle('hidden',  !isRunning)
  btnStart.disabled = false
  btnStop.disabled  = isBusy

  refreshPlayerBar(inst)
  refreshMetricsBar(inst)
}

async function playerAction(instanceId, player, action) {
  try {
    await api('POST', `/api/instances/${instanceId}/players/${encodeURIComponent(player)}/${action}`)
    showToast('Player action', `${action} sent to ${player}`, 'success', 3000)
  } catch (e) {
    showToast('Player action failed', e.message, 'error', 4000)
  }
}

function renderDashboardPlayerPanel() {
  const panel = document.getElementById('dashboard-player-panel')
  if (!panel) return
  const running = [...instances.values()].find(i => i.status === 'running' || i.status === 'starting')
  if (!running || running.players.length === 0) {
    panel.classList.add('hidden')
    return
  }
  panel.classList.remove('hidden')
  document.getElementById('dpp-server-name').textContent = running.display_name
  const isRunning = running.status === 'running'
  document.getElementById('dpp-player-tags').innerHTML = running.players.map(p => `
    <span class="player-tag-wrap">
      <span class="player-tag">${esc(p)}</span>
      ${isRunning ? `
        <span class="player-actions">
          <button class="player-action-btn" onclick="playerAction(${JSON.stringify(running.id)},${JSON.stringify(p)},'kick')" title="Kick">✕</button>
          <button class="player-action-btn" onclick="playerAction(${JSON.stringify(running.id)},${JSON.stringify(p)},'op')" title="Op">★</button>
          <button class="player-action-btn" onclick="playerAction(${JSON.stringify(running.id)},${JSON.stringify(p)},'deop')" title="Deop">☆</button>
        </span>` : ''}
    </span>`).join('')
}

function refreshPlayerBar(inst) {
  const bar = document.getElementById('player-bar')
  if (!inst || inst.players.length === 0) { bar.classList.add('hidden'); return }
  bar.classList.remove('hidden')
  const isRunning = inst.status === 'running'
  bar.innerHTML = `<span class="label">Online:</span>` + inst.players.map(p => `
    <span class="player-tag-wrap">
      <span class="player-tag">${esc(p)}</span>
      ${isRunning ? `
        <span class="player-actions">
          <button class="player-action-btn" onclick="playerAction(${JSON.stringify(inst.id)},${JSON.stringify(p)},'kick')" title="Kick">✕</button>
          <button class="player-action-btn" onclick="playerAction(${JSON.stringify(inst.id)},${JSON.stringify(p)},'op')" title="Op">★</button>
          <button class="player-action-btn" onclick="playerAction(${JSON.stringify(inst.id)},${JSON.stringify(p)},'deop')" title="Deop">☆</button>
        </span>` : ''}
    </span>`).join('')
}

function refreshMetricsBar(inst) {
  const bar = document.getElementById('metrics-bar')
  if (!bar) return
  if (!inst || inst.ram_mb == null) { bar.classList.add('hidden'); return }
  bar.classList.remove('hidden')
  const ram = inst.ram_mb >= 1024 ? `${(inst.ram_mb / 1024).toFixed(1)} GB` : `${inst.ram_mb} MB`
  let html = `<span class="metric-chip">💾 ${ram}</span>`
  if (inst.tps != null) {
    html += `<span class="metric-chip metric-tps ${tpsClass(inst.tps)}">⟳ ${inst.tps.toFixed(1)} TPS</span>`
  }
  if (inst.cpu_pct != null) {
    const cpuCls = inst.cpu_pct >= 90 ? 'metric-bad' : inst.cpu_pct >= 70 ? 'metric-warn' : ''
    html += `<span class="metric-chip ${cpuCls}">⚡ ${inst.cpu_pct.toFixed(1)}%</span>`
  }
  bar.innerHTML = html
}

function detailStart() {
  if (!detailId) return
  setDetailError('')
  document.getElementById('btn-detail-start').disabled = true
  doStart(detailId)
}

function detailStop() {
  if (!detailId) return
  setDetailError('')
  document.getElementById('btn-detail-stop').disabled = true
  doStop(detailId)
}

function setDetailError(msg) {
  const el = document.getElementById('detail-error')
  el.textContent = msg
  el.classList.toggle('hidden', !msg)
}

// ─── Tab switching ────────────────────────────────────────────────────────────
function switchTab(name) {
  document.querySelectorAll('.tab-btn').forEach(b => b.classList.toggle('active', b.dataset.tab === name))
  document.querySelectorAll('.tab-panel').forEach(p => p.classList.toggle('hidden', p.id !== 'tab-' + name))
  if (name === 'backups'    && detailId) renderBackups()
  if (name === 'mods'       && detailId) renderMods()
  if (name === 'datapacks'  && detailId) renderDatapacks()
  if (name === 'stats'      && detailId) loadStats(detailId)
  if (name === 'settings' && detailId) { loadSettings(detailId); loadDiskUsage(detailId); loadBackupConfig(detailId); loadAlertsConfig(detailId); loadSchedules(detailId) }
}

// ─── Log view ─────────────────────────────────────────────────────────────────
function linePassesFilter(line) {
  if (logFilter !== 'all') {
    const level = logLevel(line)
    if (logFilter === 'error' && level !== 'log-error') return false
    if (logFilter === 'warn'  && level !== 'log-warn' && level !== 'log-error') return false
    if (logFilter === 'info'  && level === 'log-debug') return false
  }
  if (logSearch) {
    if (logRegex) {
      try { if (!new RegExp(logSearch, 'i').test(line)) return false } catch { return false }
    } else {
      if (!line.toLowerCase().includes(logSearch.toLowerCase())) return false
    }
  }
  return true
}

function renderLogs() {
  const el = document.getElementById('log-output')
  el.innerHTML = ''
  const buf = detailId ? (logs.get(detailId) ?? []) : []
  buf.forEach(({ line, timestamp }) => appendLogLine(line, timestamp, false))
  el.scrollTop = el.scrollHeight
  logPinned = true
}

function appendLogLine(line, timestamp, autoScroll = true) {
  const el = document.getElementById('log-output')
  if (!el) return

  if (!linePassesFilter(line)) return

  const div = document.createElement('div')
  div.className = 'log-line ' + logLevel(line)
  div.innerHTML = `<span class="log-ts">${fmtTime(timestamp)}</span><span class="log-msg">${esc(line)}</span>`
  el.appendChild(div)

  while (el.children.length > 1000) el.removeChild(el.firstChild)
  if (autoScroll && logPinned) el.scrollTop = el.scrollHeight
}

function setLogFilter(level) {
  logFilter = level
  document.querySelectorAll('.log-filter-btn').forEach(b => b.classList.toggle('active', b.dataset.level === level))
  renderLogs()
}

function setLogSearch(value) {
  logSearch = value
  renderLogs()
}

function toggleRegex() {
  logRegex = !logRegex
  document.getElementById('btn-regex').classList.toggle('active', logRegex)
  renderLogs()
}

function jumpToError() {
  const el = document.getElementById('log-output')
  if (!el) return
  const lines = el.querySelectorAll('.log-error')
  if (!lines.length) return
  // Find the next error after current scroll position
  const scrollTop = el.scrollTop
  let target = lines[0]
  for (const ln of lines) {
    if (ln.offsetTop > scrollTop + 10) { target = ln; break }
  }
  target.scrollIntoView({ block: 'center' })
}

function downloadLog() {
  const buf = detailId ? (logs.get(detailId) ?? []) : []
  if (!buf.length) return
  const text = buf.map(({ line }) => line).join('\n')
  const blob = new Blob([text], { type: 'text/plain' })
  const a = document.createElement('a')
  a.href = URL.createObjectURL(blob)
  a.download = `${detailId || 'server'}-log.txt`
  a.click()
  URL.revokeObjectURL(a.href)
}

// ─── Command autocomplete ─────────────────────────────────────────────────────

const MC_COMMANDS = [
  // Server management
  { cmd: 'stop',          usage: 'stop',                                              desc: 'Stop the server gracefully' },
  { cmd: 'restart',       usage: 'restart',                                           desc: 'Restart the server (if supported)' },
  { cmd: 'save-all',      usage: 'save-all [flush]',                                  desc: 'Force-save all chunks to disk' },
  { cmd: 'save-on',       usage: 'save-on',                                           desc: 'Re-enable auto-saving' },
  { cmd: 'save-off',      usage: 'save-off',                                          desc: 'Disable auto-saving (useful before backups)' },
  { cmd: 'reload',        usage: 'reload [target]',                                   desc: 'Reload datapacks / Fabric mods' },
  { cmd: 'debug',         usage: 'debug <start|stop|report|function>',                desc: 'Control the server profiler' },
  { cmd: 'perf',          usage: 'perf <start|stop>',                                 desc: 'Start/stop performance profiling' },
  { cmd: 'jfr',           usage: 'jfr <start|stop>',                                  desc: 'Control JVM Flight Recorder' },
  // Players
  { cmd: 'list',          usage: 'list [uuids]',                                      desc: 'List online players (and optionally their UUIDs)' },
  { cmd: 'say',           usage: 'say <message>',                                     desc: 'Broadcast a message to all players' },
  { cmd: 'tell',          usage: 'tell <player> <message>',                           desc: 'Send a private message to a player' },
  { cmd: 'msg',           usage: 'msg <player> <message>',                            desc: 'Send a private message (alias for tell)' },
  { cmd: 'me',            usage: 'me <action>',                                       desc: 'Broadcast an action in chat' },
  { cmd: 'kick',          usage: 'kick <player> [reason]',                            desc: 'Kick a player from the server' },
  { cmd: 'ban',           usage: 'ban <player> [reason]',                             desc: 'Ban a player by name' },
  { cmd: 'ban-ip',        usage: 'ban-ip <address|player> [reason]',                  desc: 'Ban a player by IP address' },
  { cmd: 'pardon',        usage: 'pardon <player>',                                   desc: 'Unban a player' },
  { cmd: 'pardon-ip',     usage: 'pardon-ip <address>',                               desc: 'Unban an IP address' },
  { cmd: 'op',            usage: 'op <player>',                                       desc: 'Grant operator permissions to a player' },
  { cmd: 'deop',          usage: 'deop <player>',                                     desc: 'Revoke operator permissions from a player' },
  { cmd: 'whitelist',     usage: 'whitelist <add|remove|list|on|off|reload> [player]', desc: 'Manage the server whitelist' },
  // Teleport & world
  { cmd: 'tp',            usage: 'tp <dest> | tp <x> <y> <z> | tp <from> <to>',       desc: 'Teleport a player or entity' },
  { cmd: 'teleport',      usage: 'teleport <dest> | tp <x> <y> <z>',                 desc: 'Teleport (alias for tp)' },
  { cmd: 'spawnpoint',    usage: 'spawnpoint [player] [x y z] [angle]',               desc: 'Set a player\'s spawn point' },
  { cmd: 'setworldspawn', usage: 'setworldspawn [x y z] [angle]',                     desc: 'Set the world\'s spawn point' },
  { cmd: 'time',          usage: 'time <set|add|query> <value|day|night|noon|midnight>', desc: 'Get or change the world time' },
  { cmd: 'weather',       usage: 'weather <clear|rain|thunder> [duration]',            desc: 'Change the weather' },
  { cmd: 'difficulty',    usage: 'difficulty <peaceful|easy|normal|hard>',             desc: 'Set the game difficulty' },
  { cmd: 'gamerule',      usage: 'gamerule [rule] [value]',                            desc: 'Get or set a game rule (e.g. keepInventory, doDaylightCycle)' },
  { cmd: 'worldborder',   usage: 'worldborder <add|center|damage|get|set|warning>',    desc: 'Manage the world border' },
  { cmd: 'locate',        usage: 'locate <structure|biome|poi> <type>',                desc: 'Find the nearest structure, biome, or POI' },
  { cmd: 'spreadplayers', usage: 'spreadplayers <x z> <spread> <maxRange> <teams> <targets>', desc: 'Randomly scatter players' },
  // Game mechanics
  { cmd: 'gamemode',      usage: 'gamemode <survival|creative|adventure|spectator> [player]', desc: 'Set a player\'s game mode' },
  { cmd: 'give',          usage: 'give <player> <item> [count]',                      desc: 'Give a player items' },
  { cmd: 'clear',         usage: 'clear [player] [item] [count]',                     desc: 'Clear a player\'s inventory' },
  { cmd: 'kill',          usage: 'kill [player|entity]',                              desc: 'Kill entity/player (defaults to command executor)' },
  { cmd: 'effect',        usage: 'effect <give|clear> <target> [effect] [duration] [amp] [hide]', desc: 'Apply or remove status effects' },
  { cmd: 'enchant',       usage: 'enchant <target> <enchantment> [level]',             desc: 'Enchant the held item' },
  { cmd: 'experience',    usage: 'experience <add|set|query> <player> <amount> [points|levels]', desc: 'Modify player experience' },
  { cmd: 'xp',            usage: 'xp <add|set|query> <player> <amount>',              desc: 'Modify XP (alias for experience)' },
  { cmd: 'advancement',   usage: 'advancement <grant|revoke> <player> <everything|only|from|through|until> [advancement]', desc: 'Manage advancements' },
  { cmd: 'recipe',        usage: 'recipe <give|take> <player> (*|<recipe>)',           desc: 'Unlock or lock crafting recipes' },
  { cmd: 'attribute',     usage: 'attribute <target> <attribute> <get|base|modifier>', desc: 'Query or modify entity attributes' },
  { cmd: 'trigger',       usage: 'trigger <objective> [add|set] [value]',             desc: 'Modify a trigger scoreboard objective' },
  // Blocks & entities
  { cmd: 'setblock',      usage: 'setblock <x> <y> <z> <block> [destroy|keep|replace]', desc: 'Place a single block' },
  { cmd: 'fill',          usage: 'fill <x1 y1 z1> <x2 y2 z2> <block> [mode]',         desc: 'Fill a region with a block' },
  { cmd: 'clone',         usage: 'clone <x1 y1 z1> <x2 y2 z2> <destX destY destZ>',   desc: 'Copy blocks from one region to another' },
  { cmd: 'summon',        usage: 'summon <entity> [x y z] [nbt]',                      desc: 'Spawn an entity at a location' },
  { cmd: 'data',          usage: 'data <get|merge|modify|remove> <entity|block|storage> ...', desc: 'Manipulate NBT data' },
  { cmd: 'loot',          usage: 'loot <target> <source>',                             desc: 'Drop items from a loot table' },
  // Scoreboards & teams
  { cmd: 'scoreboard',    usage: 'scoreboard <objectives|players> ...',                desc: 'Manage scoreboard objectives and scores' },
  { cmd: 'team',          usage: 'team <add|remove|empty|join|leave|list|modify> ...',  desc: 'Manage teams' },
  { cmd: 'tag',           usage: 'tag <targets> <add|remove|list> [name]',             desc: 'Manage entity tags' },
  { cmd: 'bossbar',       usage: 'bossbar <add|get|list|remove|set> ...',              desc: 'Manage boss bars' },
  { cmd: 'title',         usage: 'title <player> <clear|reset|title|subtitle|actionbar|times> ...', desc: 'Display titles on screen' },
  // Command execution
  { cmd: 'execute',       usage: 'execute <if|unless|as|at|positioned|rotated|facing|align|anchored|in|on|store|run> ...', desc: 'Conditional/contextual command execution' },
  { cmd: 'function',      usage: 'function <namespace:path>',                          desc: 'Run all commands in a .mcfunction file' },
  { cmd: 'schedule',      usage: 'schedule <function|clear> <name> [time] [append|replace]', desc: 'Schedule a function to run later' },
  // Forge / NeoForge
  { cmd: 'forge tps',     usage: 'forge tps [dim]',                                   desc: 'Show TPS per dimension (Forge/NeoForge)' },
  { cmd: 'forge gen',     usage: 'forge gen',                                          desc: 'Show chunk generation stats (Forge/NeoForge)' },
  { cmd: 'forge track',   usage: 'forge track',                                        desc: 'Toggle entity/tile tracking report (Forge/NeoForge)' },
  { cmd: 'forge config',  usage: 'forge config load <type> <id>',                     desc: 'Reload Forge config (Forge/NeoForge)' },
  // Fabric
  { cmd: 'fabric',        usage: 'fabric <dump-registry|mods|report|...>',             desc: 'Fabric server subcommands' },
]

let _acItems = []    // filtered MC_COMMANDS for current input
let _acIdx   = -1    // currently highlighted index (-1 = none)

function _acEl() { return document.getElementById('cmd-autocomplete') }

function updateAutocomplete(raw) {
  const val = raw.trimStart()

  if (!val) { hideAutocomplete(); return }

  // Match command prefix (before first space shows command list; after space shows arg hint for that command)
  const spaceIdx = val.indexOf(' ')
  const typed = spaceIdx === -1 ? val : val.slice(0, spaceIdx)
  const lower = typed.toLowerCase()

  if (spaceIdx !== -1) {
    // User has typed a full command + space — show the single matching entry as an argument hint
    const exact = MC_COMMANDS.filter(c => c.cmd.toLowerCase() === lower)
    _acItems = exact
  } else {
    // Show all commands that start with what's typed
    _acItems = MC_COMMANDS.filter(c => c.cmd.toLowerCase().startsWith(lower))
  }

  if (!_acItems.length) { hideAutocomplete(); return }

  _acIdx = -1
  _renderAutocomplete(lower)
}

function _renderAutocomplete(highlight) {
  const el = _acEl()
  if (!el) return
  el.innerHTML = _acItems.map((item, i) => {
    const hi = esc(item.cmd).replace(
      new RegExp(`^(${esc(highlight)})`, 'i'),
      '<mark>$1</mark>'
    )
    return `<div class="ac-item${i === _acIdx ? ' ac-selected' : ''}"
                 role="option"
                 onmousedown="applyAutocomplete(${i})"
                 onmouseover="_acHover(${i})">
      <span class="ac-cmd">${hi}</span>
      <span class="ac-right">
        <span class="ac-usage">${esc(item.usage)}</span>
        <span class="ac-desc">${esc(item.desc)}</span>
      </span>
    </div>`
  }).join('')
  el.classList.remove('hidden')
}

function hideAutocomplete() {
  _acItems = []
  _acIdx   = -1
  const el = _acEl()
  if (el) el.classList.add('hidden')
}

function _acHover(i) {
  if (_acIdx === i) return
  _acIdx = i
  _acEl().querySelectorAll('.ac-item').forEach((el, j) => el.classList.toggle('ac-selected', j === i))
}

function moveAutocomplete(dir) {
  if (!_acItems.length) return false
  _acIdx = (_acIdx + dir + _acItems.length) % _acItems.length
  _acEl().querySelectorAll('.ac-item').forEach((el, i) => el.classList.toggle('ac-selected', i === _acIdx))
  const sel = _acEl().querySelector('.ac-selected')
  if (sel) sel.scrollIntoView({ block: 'nearest' })
  return true
}

function applyAutocomplete(idx) {
  const item = _acItems[idx ?? _acIdx]
  if (!item) return
  const input = document.getElementById('cmd-input')
  // If user already typed past the command, only fill up to the command name + space
  const val = input.value.trimStart()
  const spaceIdx = val.indexOf(' ')
  if (spaceIdx !== -1) {
    // Already past the command word — leave what they typed after the space
    input.value = item.cmd + ' ' + val.slice(spaceIdx + 1)
  } else {
    input.value = item.cmd + ' '
  }
  hideAutocomplete()
  input.focus()
}

document.addEventListener('DOMContentLoaded', () => {
  const logEl = document.getElementById('log-output')
  if (logEl) {
    logEl.addEventListener('scroll', () => {
      const { scrollTop, scrollHeight, clientHeight } = logEl
      logPinned = scrollHeight - scrollTop - clientHeight < 40
    })
  }

  const cmdInput = document.getElementById('cmd-input')
  if (cmdInput) {
    cmdInput.addEventListener('input', e => {
      updateAutocomplete(e.target.value)
    })

    cmdInput.addEventListener('keydown', e => {
      // Autocomplete takes priority when dropdown is visible
      if (_acItems.length) {
        if (e.key === 'ArrowUp') {
          e.preventDefault()
          moveAutocomplete(-1)
          return
        }
        if (e.key === 'ArrowDown') {
          e.preventDefault()
          moveAutocomplete(1)
          return
        }
        if (e.key === 'Tab') {
          e.preventDefault()
          applyAutocomplete(_acIdx === -1 ? 0 : _acIdx)
          return
        }
        if (e.key === 'Escape') {
          e.preventDefault()
          hideAutocomplete()
          return
        }
      } else {
        // History navigation (only when autocomplete is closed)
        if (e.key === 'ArrowUp') {
          e.preventDefault()
          if (cmdHistoryPos < cmdHistory.length - 1) {
            cmdHistoryPos++
            cmdInput.value = cmdHistory[cmdHistoryPos]
            setTimeout(() => cmdInput.setSelectionRange(cmdInput.value.length, cmdInput.value.length), 0)
          }
          return
        }
        if (e.key === 'ArrowDown') {
          e.preventDefault()
          if (cmdHistoryPos > 0) {
            cmdHistoryPos--
            cmdInput.value = cmdHistory[cmdHistoryPos]
          } else if (cmdHistoryPos === 0) {
            cmdHistoryPos = -1
            cmdInput.value = ''
          }
          return
        }
      }
      if (e.key !== 'Enter' && e.key !== 'Tab') cmdHistoryPos = -1
    })

    // Close autocomplete on blur (slight delay so mousedown on item fires first)
    cmdInput.addEventListener('blur', () => setTimeout(hideAutocomplete, 120))
  }
})

function submitCmd(e) {
  e.preventDefault()
  const input = document.getElementById('cmd-input')
  const cmd = input.value.trim()
  if (!cmd || !detailId) return
  input.value = ''
  cmdHistoryPos = -1
  hideAutocomplete()
  if (cmdHistory[0] !== cmd) {
    cmdHistory.unshift(cmd)
    if (cmdHistory.length > 100) cmdHistory.pop()
    localStorage.setItem('cmd_history', JSON.stringify(cmdHistory))
  }
  api('POST', `/api/instances/${detailId}/cmd`, { command: cmd }).catch(() => {})
}

// ─── Add Instance modal ───────────────────────────────────────────────────────
function openAddModal() {
  document.getElementById('add-form').reset()
  document.getElementById('id-preview').textContent = ''
  document.getElementById('add-error').classList.add('hidden')
  document.getElementById('modal-backdrop').classList.remove('hidden')
}

function closeAddModal() {
  document.getElementById('modal-backdrop').classList.add('hidden')
}

function backdropClose(e) {
  if (e.target === e.currentTarget) closeAddModal()
}

function updateIdPreview() {
  const name = document.getElementById('add-name').value
  const id   = slugify(name)
  document.getElementById('id-preview').textContent = id ? `ID: ${id}` : ''
}

async function submitAdd(e) {
  e.preventDefault()
  const btn      = document.getElementById('btn-add-submit')
  const errorEl  = document.getElementById('add-error')
  btn.disabled   = true
  btn.textContent = 'Adding…'
  errorEl.classList.add('hidden')

  const javaPath = document.getElementById('add-java').value.trim()
  const data = {
    id:                slugify(document.getElementById('add-name').value),
    display_name:      document.getElementById('add-name').value.trim(),
    server_path:       document.getElementById('add-path').value.trim(),
    minecraft_version: document.getElementById('add-ver').value.trim(),
    port:              parseInt(document.getElementById('add-port').value, 10),
    java_path:         javaPath || null,
  }

  try {
    const inst = await api('POST', '/api/instances', data)
    instances.set(inst.id, inst)
    logs.set(inst.id, [])
    renderDashboard()
    closeAddModal()
  } catch (err) {
    errorEl.textContent = err.message
    errorEl.classList.remove('hidden')
  } finally {
    btn.disabled    = false
    btn.textContent = 'Add Instance'
  }
}

// ─── Backups ──────────────────────────────────────────────────────────────────
async function loadBackups(id) {
  try {
    const list = await api('GET', `/api/instances/${id}/backups`)
    backups.set(id, list)
    if (detailId === id) renderBackups()
  } catch { /* silent — instance may not exist yet */ }
}

function renderBackups() {
  const list = detailId ? (backups.get(detailId) ?? null) : null
  const container = document.getElementById('backup-list')
  if (!container) return

  if (list === null) {
    container.innerHTML = '<div class="placeholder">Loading…</div>'
    return
  }
  if (list.length === 0) {
    container.innerHTML = '<div class="placeholder">No backups yet.</div>'
    return
  }
  container.innerHTML = list.map(b => backupRowHTML(b)).join('')
}

function backupRowHTML(b) {
  const name = esc(b.filename)
  const enc  = encodeURIComponent(b.filename)
  const otherInstances = [...instances.values()].filter(i => i.id !== detailId)
  const copyOpts = otherInstances.length
    ? otherInstances.map(i => `<option value="${esc(i.id)}">${esc(i.display_name)}</option>`).join('')
    : ''
  const copyControl = otherInstances.length
    ? `<span class="backup-copy-wrap">
        <select class="backup-copy-select" id="copy-sel-${enc}">
          <option value="">Copy to…</option>
          ${copyOpts}
        </select>
        <button class="btn-icon" title="Copy" onclick="doCopyBackup('${name}', '${enc}')">⧉</button>
      </span>`
    : ''
  return `<div class="backup-row" id="backup-row-${enc}">
    <div class="backup-info">
      <span class="backup-filename">${name}</span>
      <span class="backup-meta">${fmtSize(b.size_bytes)} &middot; ${fmtDate(b.created_at)}</span>
    </div>
    <div class="backup-actions">
      ${copyControl}
      <a class="btn-icon" title="Download" href="/api/instances/${esc(detailId)}/backups/${enc}/download" download="${name}">⬇</a>
      <button class="btn-icon btn-restore" title="Restore" onclick="doRestore('${name}')">↺</button>
      <button class="btn-icon btn-danger" title="Delete" onclick="doDeleteBackup('${name}', '${enc}')">✕</button>
    </div>
  </div>`
}

async function doDeleteBackup(filename, enc) {
  if (!detailId) return
  if (!confirm(`Delete backup "${filename}"?\n\nThis cannot be undone.`)) return
  const row = document.getElementById('backup-row-' + enc)
  if (row) row.style.opacity = '0.4'
  try {
    await api('DELETE', `/api/instances/${detailId}/backups/${enc}`)
    if (row) row.remove()
    const list = backups.get(detailId) ?? []
    backups.set(detailId, list.filter(b => b.filename !== filename))
    if ((backups.get(detailId) ?? []).length === 0) renderBackups()
  } catch (e) {
    if (row) row.style.opacity = ''
    setBackupMsg('Delete failed: ' + e.message, 'error')
  }
}

async function doCopyBackup(filename, enc) {
  if (!detailId) return
  const sel = document.getElementById('copy-sel-' + enc)
  const target = sel?.value
  if (!target) return
  const targetName = instances.get(target)?.display_name ?? target
  if (!confirm(`Copy "${filename}" to "${targetName}"?`)) { sel.value = ''; return }
  try {
    await api('POST', `/api/instances/${detailId}/backups/${enc}/copy`, { target_instance_id: target })
    sel.value = ''
    setBackupMsg(`Copied to ${targetName}.`, 'success')
  } catch (e) {
    sel.value = ''
    setBackupMsg('Copy failed: ' + e.message, 'error')
  }
}

async function doCreateBackup() {
  if (!detailId) return
  const btn = document.getElementById('btn-create-backup')
  btn.disabled = true
  setBackupMsg('Backup in progress…', '')
  try {
    await api('POST', `/api/instances/${detailId}/backups`)
    // BackupDone SSE event will re-enable the button and update the list
  } catch (e) {
    setBackupMsg('Backup failed: ' + e.message, 'error')
    btn.disabled = false
  }
}

async function doRestore(filename) {
  if (!detailId) return
  if (!confirm(`Restore "${filename}"?\n\nThis overwrites all server files with the backup contents. The instance must be stopped first.`)) return
  const btns = document.querySelectorAll('.btn-restore')
  btns.forEach(b => b.disabled = true)
  try {
    await api('POST', `/api/instances/${detailId}/backups/${encodeURIComponent(filename)}/restore`)
    setBackupMsg('Restore complete.', 'success')
  } catch (e) {
    setBackupMsg('Restore failed: ' + e.message, 'error')
  } finally {
    btns.forEach(b => b.disabled = false)
  }
}

let _backupMsgTimer = null
function setBackupMsg(text, type) {
  const el = document.getElementById('backup-status-msg')
  if (!el) return
  el.textContent = text
  el.className = 'backup-msg' + (type ? ' ' + type : '')
  el.classList.remove('hidden')
  if (_backupMsgTimer) clearTimeout(_backupMsgTimer)
  if (type === 'success') {
    _backupMsgTimer = setTimeout(() => el.classList.add('hidden'), 8000)
  }
}

// ─── Mods ─────────────────────────────────────────────────────────────────────
async function loadMods(id) {
  try {
    const mods = await api('GET', `/api/instances/${id}/mods`)
    modsData.set(id, { mods: mods ?? [], updates: modsData.get(id)?.updates ?? null })
    if (detailId === id) renderMods()
  } catch { /* silent */ }
}

function renderMods() {
  const data = detailId ? (modsData.get(detailId) ?? null) : null
  const container = document.getElementById('mod-list')
  if (!container) return

  const btnCheck = document.getElementById('btn-check-updates')
  const btnAll   = document.getElementById('btn-update-all')

  if (data === null) {
    container.innerHTML = '<div class="placeholder">Loading…</div>'
    return
  }

  const { mods, updates } = data
  const updateMap = updates ? Object.fromEntries(updates.map(u => [u.project_id, u])) : null

  if (mods.length === 0) {
    container.innerHTML = `<div class="mods-empty">
      <p>No mods found in the lock file.</p>
      <p class="mods-empty-hint">Click <strong>Scan Mods</strong> to detect mods from the server's <code>mods/</code> directory.</p>
    </div>`
    btnCheck.disabled = true
    btnAll.disabled   = true
    return
  }

  btnCheck.disabled = false
  const updateCount = updates ? updates.length : 0
  btnAll.disabled   = updateCount === 0
  btnAll.textContent = updateCount > 0 ? `Update All (${updateCount})` : 'Update All'

  container.innerHTML = mods.map(m => modRowHTML(m, updateMap ? updateMap[m.modrinth_project_id] : null)).join('')
}

function modRowHTML(mod, update) {
  const hasUpdate = !!update
  const statusClass = update === undefined ? '' : (hasUpdate ? 'has-update' : 'up-to-date')
  let versionCol = ''

  if (update === undefined) {
    // updates not yet checked
    versionCol = `<span class="mod-version">${esc(mod.version_number)}</span>`
  } else if (hasUpdate) {
    versionCol = `
      <span class="mod-version">${esc(mod.version_number)}</span>
      <span class="mod-arrow">→</span>
      <span class="mod-new-version">${esc(update.latest_version_number)}</span>`
  } else {
    versionCol = `
      <span class="mod-version">${esc(mod.version_number)}</span>
      <span class="mod-uptodate">✓</span>`
  }

  const updateBtn = hasUpdate
    ? `<button class="btn-outline btn-mod-update" onclick="doUpdateMod('${esc(mod.modrinth_project_id)}', this)">Update</button>`
    : ''

  return `<div class="mod-row ${statusClass}">
    <div class="mod-name">${esc(mod.name)}</div>
    <div class="mod-version-cell">${versionCol}</div>
    ${updateBtn}
  </div>`
}

async function doScanMods() {
  if (!detailId) return
  const btn = document.getElementById('btn-scan-mods')
  btn.disabled = true
  setModsMsg('Scanning mods directory…', '')
  try {
    const mods = await api('POST', `/api/instances/${detailId}/mods`)
    modsData.set(detailId, { mods: mods ?? [], updates: null })
    renderMods()
    setModsMsg(`Found ${mods.length} mod${mods.length !== 1 ? 's' : ''} on Modrinth.`, 'success')
  } catch (e) {
    setModsMsg('Scan failed: ' + e.message, 'error')
  } finally {
    btn.disabled = false
  }
}

async function doCheckUpdates() {
  if (!detailId) return
  const btn = document.getElementById('btn-check-updates')
  btn.disabled = true
  setModsMsg('Checking Modrinth for updates…', '')
  try {
    const updates = await api('GET', `/api/instances/${detailId}/mods/updates`)
    const data = modsData.get(detailId) ?? { mods: [], updates: null }
    modsData.set(detailId, { ...data, updates: updates ?? [] })
    renderMods()
    const n = (updates ?? []).length
    setModsMsg(n > 0 ? `${n} update${n !== 1 ? 's' : ''} available.` : 'All mods are up to date.', n > 0 ? '' : 'success')
  } catch (e) {
    setModsMsg('Update check failed: ' + e.message, 'error')
  } finally {
    btn.disabled = false
  }
}

async function doUpdateMod(projectId, btnEl) {
  if (!detailId) return
  btnEl.disabled = true
  setModsMsg('Updating mod…', '')
  try {
    await api('POST', `/api/instances/${detailId}/mods/${encodeURIComponent(projectId)}/update`)
    // Re-fetch mods and clear updates so user runs check again for fresh state
    const mods = await api('GET', `/api/instances/${detailId}/mods`)
    const data = modsData.get(detailId)
    const newUpdates = data?.updates?.filter(u => u.project_id !== projectId) ?? null
    modsData.set(detailId, { mods: mods ?? [], updates: newUpdates })
    renderMods()
    setModsMsg('Mod updated.', 'success')
  } catch (e) {
    setModsMsg('Update failed: ' + e.message, 'error')
    btnEl.disabled = false
  }
}

async function doUpdateAll() {
  if (!detailId) return
  const btn = document.getElementById('btn-update-all')
  btn.disabled = true
  setModsMsg('Updating all mods…', '')
  try {
    await api('POST', `/api/instances/${detailId}/mods/update-all`)
    const mods = await api('GET', `/api/instances/${detailId}/mods`)
    modsData.set(detailId, { mods: mods ?? [], updates: [] })
    renderMods()
    setModsMsg('All mods updated.', 'success')
  } catch (e) {
    setModsMsg('Update failed: ' + e.message, 'error')
    btn.disabled = false
  }
}

let _modsMsgTimer = null
function setModsMsg(text, type) {
  const el = document.getElementById('mods-status-msg')
  if (!el) return
  el.textContent = text
  el.className = 'mods-msg' + (type ? ' ' + type : '')
  el.classList.remove('hidden')
  if (_modsMsgTimer) clearTimeout(_modsMsgTimer)
  if (type === 'success') {
    _modsMsgTimer = setTimeout(() => el.classList.add('hidden'), 8000)
  }
}

// ─── Datapacks ────────────────────────────────────────────────────────────────
async function loadDatapacks(id) {
  try {
    const datapacks = await api('GET', `/api/instances/${id}/datapacks`)
    datapacksData.set(id, { datapacks: datapacks ?? [], updates: datapacksData.get(id)?.updates ?? null })
    if (detailId === id) renderDatapacks()
  } catch { /* silent */ }
}

function renderDatapacks() {
  const data = detailId ? (datapacksData.get(detailId) ?? null) : null
  const container = document.getElementById('datapack-list')
  if (!container) return

  const btnCheck = document.getElementById('btn-check-dp-updates')
  const btnAll   = document.getElementById('btn-update-all-dp')

  if (data === null) {
    container.innerHTML = '<div class="placeholder">Loading…</div>'
    return
  }

  const { datapacks, updates } = data
  const updateMap = updates ? Object.fromEntries(updates.map(u => [u.project_id, u])) : null

  if (datapacks.length === 0) {
    container.innerHTML = `<div class="mods-empty">
      <p>No datapacks found in the lock file.</p>
      <p class="mods-empty-hint">Click <strong>Scan Datapacks</strong> to detect datapacks from the world's <code>datapacks/</code> directory.</p>
    </div>`
    btnCheck.disabled = true
    btnAll.disabled   = true
    return
  }

  btnCheck.disabled = false
  const updateCount = updates ? updates.length : 0
  btnAll.disabled   = updateCount === 0
  btnAll.textContent = updateCount > 0 ? `Update All (${updateCount})` : 'Update All'

  container.innerHTML = datapacks.map(d => datapackRowHTML(d, updateMap ? updateMap[d.modrinth_project_id] : null)).join('')
}

function datapackRowHTML(dp, update) {
  const hasUpdate = !!update
  const statusClass = update === undefined ? '' : (hasUpdate ? 'has-update' : 'up-to-date')
  let versionCol = ''

  if (update === undefined) {
    versionCol = `<span class="mod-version">${esc(dp.version_number)}</span>`
  } else if (hasUpdate) {
    versionCol = `
      <span class="mod-version">${esc(dp.version_number)}</span>
      <span class="mod-arrow">→</span>
      <span class="mod-new-version">${esc(update.latest_version_number)}</span>`
  } else {
    versionCol = `
      <span class="mod-version">${esc(dp.version_number)}</span>
      <span class="mod-uptodate">✓</span>`
  }

  const updateBtn = hasUpdate
    ? `<button class="btn-outline btn-mod-update" onclick="doUpdateDatapack('${esc(dp.modrinth_project_id)}', this)">Update</button>`
    : ''

  return `<div class="mod-row ${statusClass}">
    <div class="mod-name">${esc(dp.name)}</div>
    <div class="mod-version-cell">${versionCol}</div>
    ${updateBtn}
  </div>`
}

async function doScanDatapacks() {
  if (!detailId) return
  const btn = document.getElementById('btn-scan-datapacks')
  btn.disabled = true
  setDatapacksMsg('Scanning datapacks directory…', '')
  try {
    const datapacks = await api('POST', `/api/instances/${detailId}/datapacks`)
    datapacksData.set(detailId, { datapacks: datapacks ?? [], updates: null })
    renderDatapacks()
    setDatapacksMsg(`Found ${datapacks.length} datapack${datapacks.length !== 1 ? 's' : ''} on Modrinth.`, 'success')
  } catch (e) {
    setDatapacksMsg('Scan failed: ' + e.message, 'error')
  } finally {
    btn.disabled = false
  }
}

async function doCheckDatapackUpdates() {
  if (!detailId) return
  const btn = document.getElementById('btn-check-dp-updates')
  btn.disabled = true
  setDatapacksMsg('Checking Modrinth for updates…', '')
  try {
    const updates = await api('GET', `/api/instances/${detailId}/datapacks/updates`)
    const data = datapacksData.get(detailId) ?? { datapacks: [], updates: null }
    datapacksData.set(detailId, { ...data, updates: updates ?? [] })
    renderDatapacks()
    const n = (updates ?? []).length
    setDatapacksMsg(n > 0 ? `${n} update${n !== 1 ? 's' : ''} available.` : 'All datapacks are up to date.', n > 0 ? '' : 'success')
  } catch (e) {
    setDatapacksMsg('Update check failed: ' + e.message, 'error')
  } finally {
    btn.disabled = false
  }
}

async function doUpdateDatapack(projectId, btnEl) {
  if (!detailId) return
  btnEl.disabled = true
  setDatapacksMsg('Updating datapack…', '')
  try {
    await api('POST', `/api/instances/${detailId}/datapacks/${encodeURIComponent(projectId)}/update`)
    const datapacks = await api('GET', `/api/instances/${detailId}/datapacks`)
    const data = datapacksData.get(detailId)
    const newUpdates = data?.updates?.filter(u => u.project_id !== projectId) ?? null
    datapacksData.set(detailId, { datapacks: datapacks ?? [], updates: newUpdates })
    renderDatapacks()
    setDatapacksMsg('Datapack updated.', 'success')
  } catch (e) {
    setDatapacksMsg('Update failed: ' + e.message, 'error')
    btnEl.disabled = false
  }
}

async function doUpdateAllDatapacks() {
  if (!detailId) return
  const btn = document.getElementById('btn-update-all-dp')
  btn.disabled = true
  setDatapacksMsg('Updating all datapacks…', '')
  try {
    await api('POST', `/api/instances/${detailId}/datapacks/update-all`)
    const datapacks = await api('GET', `/api/instances/${detailId}/datapacks`)
    datapacksData.set(detailId, { datapacks: datapacks ?? [], updates: [] })
    renderDatapacks()
    setDatapacksMsg('All datapacks updated.', 'success')
  } catch (e) {
    setDatapacksMsg('Update failed: ' + e.message, 'error')
    btn.disabled = false
  }
}

let _dpMsgTimer = null
function setDatapacksMsg(text, type) {
  const el = document.getElementById('datapacks-status-msg')
  if (!el) return
  el.textContent = text
  el.className = 'mods-msg' + (type ? ' ' + type : '')
  el.classList.remove('hidden')
  if (_dpMsgTimer) clearTimeout(_dpMsgTimer)
  if (type === 'success') {
    _dpMsgTimer = setTimeout(() => el.classList.add('hidden'), 8000)
  }
}

let _addDpResults = []
let _addDpSelectedHit = null

function openAddDatapackModal() {
  _addDpResults = []
  _addDpSelectedHit = null
  document.getElementById('add-dp-search-input').value = ''
  document.getElementById('add-dp-results').innerHTML = ''
  document.getElementById('add-dp-results').classList.add('hidden')
  document.getElementById('add-dp-selected').classList.add('hidden')
  document.getElementById('add-dp-error').classList.add('hidden')
  document.getElementById('btn-add-dp-submit').disabled = true
  document.getElementById('add-dp-modal-backdrop').classList.remove('hidden')
  setTimeout(() => document.getElementById('add-dp-search-input').focus(), 50)
}

function closeAddDatapackModal() {
  document.getElementById('add-dp-modal-backdrop').classList.add('hidden')
}

function addDpBackdropClose(e) {
  if (e.target === document.getElementById('add-dp-modal-backdrop')) closeAddDatapackModal()
}

async function searchModrinthDatapacks() {
  if (!detailId) return
  const term = document.getElementById('add-dp-search-input').value.trim()
  if (!term) return

  const btn = document.getElementById('btn-add-dp-search')
  const errEl = document.getElementById('add-dp-error')
  const resultsEl = document.getElementById('add-dp-results')
  btn.disabled = true
  btn.textContent = '…'
  errEl.classList.add('hidden')
  document.getElementById('add-dp-selected').classList.add('hidden')
  document.getElementById('btn-add-dp-submit').disabled = true
  resultsEl.innerHTML = '<div class="ftb-result-item" style="color:var(--text-dim)">Searching…</div>'
  resultsEl.classList.remove('hidden')

  try {
    const data = await api('GET', `/api/instances/${detailId}/datapacks/search?term=${encodeURIComponent(term)}`)
    _addDpResults = data ?? []

    if (_addDpResults.length === 0) {
      resultsEl.innerHTML = '<div class="ftb-result-item" style="color:var(--text-dim)">No results found.</div>'
      return
    }

    resultsEl.innerHTML = _addDpResults.map((hit, i) => {
      const dl = hit.downloads >= 1_000_000
        ? `${(hit.downloads / 1_000_000).toFixed(1)}M`
        : hit.downloads >= 1000
          ? `${Math.round(hit.downloads / 1000)}k`
          : String(hit.downloads)
      return `
        <div class="ftb-result-item" onclick="selectAddDpHit(${i})">
          <div class="ftb-result-name">${esc(hit.title)} <span class="mod-dl-count">${dl} downloads</span></div>
          <div class="ftb-result-desc">${esc(hit.description)}</div>
        </div>`
    }).join('')
  } catch (err) {
    errEl.textContent = err.message
    errEl.classList.remove('hidden')
    resultsEl.classList.add('hidden')
  } finally {
    btn.disabled = false
    btn.textContent = 'Search'
  }
}

async function selectAddDpHit(index) {
  _addDpSelectedHit = _addDpResults[index]

  document.querySelectorAll('#add-dp-results .ftb-result-item').forEach((el, i) => {
    el.classList.toggle('selected', i === index)
  })

  const errEl = document.getElementById('add-dp-error')
  errEl.classList.add('hidden')
  document.getElementById('add-dp-title').textContent = _addDpSelectedHit.title
  document.getElementById('add-dp-desc').textContent = _addDpSelectedHit.description
  document.getElementById('add-dp-selected').classList.remove('hidden')
  document.getElementById('btn-add-dp-submit').disabled = true

  const select = document.getElementById('add-dp-version')
  select.innerHTML = '<option>Loading versions…</option>'

  try {
    const inst = instances.get(detailId)
    const mcVer = inst?.minecraft_version ?? ''
    const versionsParam = encodeURIComponent(JSON.stringify([mcVer]))

    const versions = await fetch(
      `https://api.modrinth.com/v2/project/${encodeURIComponent(_addDpSelectedHit.project_id)}/version?game_versions=${versionsParam}`,
      { headers: { 'User-Agent': 'msm/0.1' } }
    ).then(r => {
      if (!r.ok) throw new Error(`HTTP ${r.status}`)
      return r.json()
    })

    if (!versions.length) {
      select.innerHTML = '<option value="">No compatible versions</option>'
      errEl.textContent = `No versions compatible with MC ${mcVer}`
      errEl.classList.remove('hidden')
      return
    }

    select.innerHTML = versions.map(v =>
      `<option value="${esc(v.id)}">${esc(v.name || v.version_number)} — ${esc(v.version_number)}</option>`
    ).join('')
    document.getElementById('btn-add-dp-submit').disabled = false
  } catch (err) {
    select.innerHTML = ''
    errEl.textContent = 'Failed to load versions: ' + err.message
    errEl.classList.remove('hidden')
  }
}

async function submitAddDatapack() {
  if (!detailId || !_addDpSelectedHit) return
  const versionId = document.getElementById('add-dp-version').value
  if (!versionId) return

  const btn = document.getElementById('btn-add-dp-submit')
  const errEl = document.getElementById('add-dp-error')
  btn.disabled = true
  btn.textContent = 'Adding…'
  errEl.classList.add('hidden')

  try {
    await api('POST', `/api/instances/${detailId}/datapacks/add`, {
      project_id: _addDpSelectedHit.project_id,
      version_id: versionId,
    })
    const datapacks = await api('GET', `/api/instances/${detailId}/datapacks`)
    datapacksData.set(detailId, { datapacks: datapacks ?? [], updates: null })
    renderDatapacks()
    closeAddDatapackModal()
    setDatapacksMsg(`Added ${_addDpSelectedHit.title}.`, 'success')
  } catch (err) {
    errEl.textContent = 'Failed to add datapack: ' + err.message
    errEl.classList.remove('hidden')
    btn.disabled = false
    btn.textContent = 'Add Datapack'
  }
}

// ─── Setup wizard ────────────────────────────────────────────────────────────
let _setupServerPath = null
let _setupName       = null
let _setupPort       = 25565

function openSetupModal() {
  document.getElementById('setup-form').reset()
  document.getElementById('setup-error').classList.add('hidden')
  document.getElementById('setup-mc-preview').textContent = ''
  document.getElementById('setup-step-1').classList.remove('hidden')
  document.getElementById('setup-step-2').classList.add('hidden')
  document.getElementById('setup-log-output').innerHTML = ''
  document.getElementById('setup-progress-msg').textContent = ''
  document.getElementById('btn-setup-done').classList.add('hidden')
  document.getElementById('btn-setup-close').classList.add('hidden')
  document.getElementById('btn-setup-submit').disabled = false
  document.getElementById('btn-setup-submit').textContent = 'Download & Install'
  _setupServerPath = null
  document.getElementById('setup-modal-backdrop').classList.remove('hidden')
}

function closeSetupModal() {
  document.getElementById('setup-modal-backdrop').classList.add('hidden')
}

function setupBackdropClose(e) {
  if (e.target === e.currentTarget) closeSetupModal()
}

function updateMcPreview() {
  const ver = document.getElementById('setup-nf-ver').value.trim()
  const mc  = neoForgeToMcVersion(ver)
  document.getElementById('setup-mc-preview').textContent = mc ? `MC ${mc}` : ''
}

function neoForgeToMcVersion(nfVer) {
  const parts = nfVer.split('.')
  if (parts.length < 2 || isNaN(parts[0])) return ''
  const minor = parseInt(parts[1])
  return minor === 0 ? `1.${parts[0]}` : `1.${parts[0]}.${parts[1]}`
}

async function submitSetup(e) {
  e.preventDefault()
  const btn     = document.getElementById('btn-setup-submit')
  const errorEl = document.getElementById('setup-error')
  btn.disabled  = true
  btn.textContent = 'Installing…'
  errorEl.classList.add('hidden')

  _setupName       = document.getElementById('setup-name').value.trim()
  _setupServerPath = document.getElementById('setup-dir').value.trim()
  _setupPort       = parseInt(document.getElementById('setup-port').value, 10)
  const nfVer      = document.getElementById('setup-nf-ver').value.trim()

  // Switch to progress view
  document.getElementById('setup-step-1').classList.add('hidden')
  document.getElementById('setup-step-2').classList.remove('hidden')
  document.getElementById('setup-progress-msg').textContent = 'Installing NeoForge…'

  try {
    await api('POST', '/api/setup/install-neoforge', { version: nfVer, server_path: _setupServerPath })
    // Progress comes via SSE — wait for setup_done / setup_failed events
  } catch (err) {
    onSetupFailed(err.message)
  }
}

function appendSetupLog(msg) {
  const el = document.getElementById('setup-log-output')
  if (!el) return
  const line = document.createElement('div')
  line.className = 'setup-log-line'
  line.textContent = msg
  el.appendChild(line)
  el.scrollTop = el.scrollHeight
}

function onSetupDone(serverPath) {
  _setupServerPath = serverPath
  document.getElementById('setup-progress-msg').textContent = 'Installation complete!'
  document.getElementById('btn-setup-done').classList.remove('hidden')
  document.getElementById('btn-setup-close').classList.remove('hidden')
  appendSetupLog('✓ Server files ready.')
}

function onSetupFailed(error) {
  document.getElementById('setup-progress-msg').textContent = ''
  document.getElementById('btn-setup-close').classList.remove('hidden')
  const errEl = document.createElement('div')
  errEl.className = 'setup-log-line setup-log-error'
  errEl.textContent = '✗ ' + error
  document.getElementById('setup-log-output').appendChild(errEl)
}

async function finishSetup() {
  const btn = document.getElementById('btn-setup-done')
  btn.disabled = true
  btn.textContent = 'Adding…'
  const mcVer = neoForgeToMcVersion(document.getElementById('setup-nf-ver').value.trim()) ||
                document.getElementById('setup-nf-ver').value.trim()
  try {
    const inst = await api('POST', '/api/instances', {
      id:                slugify(_setupName),
      display_name:      _setupName,
      server_path:       _setupServerPath,
      minecraft_version: mcVer,
      port:              _setupPort,
    })
    instances.set(inst.id, inst)
    logs.set(inst.id, [])
    renderDashboard()
    closeSetupModal()
  } catch (err) {
    btn.disabled = false
    btn.textContent = 'Add to MSM'
    document.getElementById('setup-progress-msg').textContent = 'Failed: ' + err.message
  }
}

// ─── Import Modpack modal ─────────────────────────────────────────────────────

function openImportModal() {
  document.getElementById('import-slug').value = ''
  document.getElementById('import-name').value = ''
  document.getElementById('import-dir').value = ''
  document.getElementById('import-port').value = '25565'
  document.getElementById('import-project-preview').classList.add('hidden')
  document.getElementById('import-version-field').classList.add('hidden')
  document.getElementById('import-version').innerHTML = ''
  document.getElementById('import-error').classList.add('hidden')
  document.getElementById('import-step-1').classList.remove('hidden')
  document.getElementById('import-step-2').classList.add('hidden')
  document.getElementById('import-log-output').innerHTML = ''
  document.getElementById('import-progress-msg').textContent = ''
  document.getElementById('btn-import-close').classList.add('hidden')
  document.getElementById('btn-import-submit').disabled = true
  document.getElementById('import-modal-backdrop').classList.remove('hidden')
}

function closeImportModal() {
  document.getElementById('import-modal-backdrop').classList.add('hidden')
}

function importBackdropClose(e) {
  if (e.target === document.getElementById('import-modal-backdrop')) closeImportModal()
}

function importSlugChanged() {
  document.getElementById('import-project-preview').classList.add('hidden')
  document.getElementById('import-version-field').classList.add('hidden')
  document.getElementById('btn-import-submit').disabled = true
  document.getElementById('import-error').classList.add('hidden')
}

function updateImportDirHint() {
  const name = document.getElementById('import-name').value.trim()
  const dir  = document.getElementById('import-dir').value.trim()
  const hint = document.getElementById('import-dir-hint')
  if (dir || !name) {
    hint.textContent = ''
  } else {
    const id = slugify(name)
    hint.textContent = `Default: ~/.local/share/msm/servers/${id}/`
  }
}

async function fetchModpackVersions() {
  const raw = document.getElementById('import-slug').value.trim()
  if (!raw) return

  // Extract slug from full Modrinth URL if pasted
  const match = raw.match(/modrinth\.com\/modpack\/([^/?#]+)/)
  const slug = match ? match[1] : raw

  const btn = document.getElementById('btn-import-fetch')
  const errEl = document.getElementById('import-error')
  btn.disabled = true
  btn.textContent = '…'
  errEl.classList.add('hidden')

  try {
    const project = await fetch(`https://api.modrinth.com/v2/project/${encodeURIComponent(slug)}`, {
      headers: { 'User-Agent': 'msm/0.1' }
    }).then(r => {
      if (!r.ok) throw new Error(`Project not found (HTTP ${r.status})`)
      return r.json()
    })

    if (project.project_type !== 'modpack') {
      throw new Error(`"${project.title}" is not a modpack (type: ${project.project_type})`)
    }

    const versions = await fetch(`https://api.modrinth.com/v2/project/${encodeURIComponent(slug)}/version`, {
      headers: { 'User-Agent': 'msm/0.1' }
    }).then(r => r.json())

    // Keep only versions that have a primary mrpack file
    const serverVersions = versions.filter(v =>
      v.files && v.files.some(f => f.primary && f.filename.endsWith('.mrpack'))
    )
    if (serverVersions.length === 0) throw new Error('No server-compatible versions found')

    document.getElementById('import-project-name').textContent = project.title
    document.getElementById('import-project-desc').textContent = project.description || ''
    document.getElementById('import-project-preview').classList.remove('hidden')

    if (!document.getElementById('import-name').value.trim()) {
      document.getElementById('import-name').value = project.title
    }

    const select = document.getElementById('import-version')
    select.innerHTML = serverVersions.map(v => {
      const mc = v.game_versions.slice(0, 2).join(', ') + (v.game_versions.length > 2 ? '…' : '')
      const loader = v.loaders.join(', ')
      const tag = v.version_type !== 'release' ? ` [${v.version_type}]` : ''
      return `<option value="${esc(v.id)}">${esc(v.name)} — MC ${esc(mc)} | ${esc(loader)}${tag}</option>`
    }).join('')

    document.getElementById('import-version-field').classList.remove('hidden')
    document.getElementById('btn-import-submit').disabled = false
  } catch (err) {
    errEl.textContent = err.message
    errEl.classList.remove('hidden')
  } finally {
    btn.disabled = false
    btn.textContent = 'Fetch'
  }
}

async function submitImport() {
  const versionId = document.getElementById('import-version').value
  const name = document.getElementById('import-name').value.trim()
  const dir = document.getElementById('import-dir').value.trim()
  const port = parseInt(document.getElementById('import-port').value, 10) || 25565

  const errEl = document.getElementById('import-error')
  if (!versionId || !name) {
    errEl.textContent = 'Please fill in all required fields.'
    errEl.classList.remove('hidden')
    return
  }

  document.getElementById('import-step-1').classList.add('hidden')
  document.getElementById('import-step-2').classList.remove('hidden')
  document.getElementById('import-progress-msg').textContent = 'Starting import…'

  try {
    await api('POST', '/api/setup/import-modpack', { version_id: versionId, server_path: dir, instance_name: name, port })
    // Progress via SSE — wait for modpack_done / modpack_failed
  } catch (err) {
    appendImportLog('✗ ' + err.message, true)
    document.getElementById('import-progress-msg').textContent = ''
    document.getElementById('btn-import-close').classList.remove('hidden')
  }
}

function appendImportLog(msg, isError = false) {
  const el = document.getElementById('import-log-output')
  const line = document.createElement('div')
  line.className = 'setup-log-line' + (isError ? ' setup-log-error' : '')
  line.textContent = msg
  el.appendChild(line)
  el.scrollTop = el.scrollHeight
}

function onModpackDone() {
  document.getElementById('import-progress-msg').textContent = 'Import complete!'
  appendImportLog('✓ Server is ready — find it in the dashboard.')
  document.getElementById('btn-import-close').classList.remove('hidden')
}

function onModpackFailed(error) {
  appendImportLog('✗ ' + error, true)
  document.getElementById('import-progress-msg').textContent = ''
  document.getElementById('btn-import-close').classList.remove('hidden')
}

// ─── FTB Import modal ─────────────────────────────────────────────────────────

let ftbResults = []
let ftbSelectedPack = null

function openFtbModal() {
  ftbResults = []
  ftbSelectedPack = null
  document.getElementById('ftb-search-input').value = ''
  document.getElementById('ftb-results').innerHTML = ''
  document.getElementById('ftb-results').classList.add('hidden')
  document.getElementById('ftb-selected').classList.add('hidden')
  document.getElementById('ftb-name').value = ''
  document.getElementById('ftb-dir').value = ''
  document.getElementById('ftb-port').value = '25565'
  document.getElementById('ftb-error').classList.add('hidden')
  document.getElementById('ftb-step-1').classList.remove('hidden')
  document.getElementById('ftb-step-2').classList.add('hidden')
  document.getElementById('ftb-log-output').innerHTML = ''
  document.getElementById('ftb-progress-msg').textContent = ''
  document.getElementById('btn-ftb-close').classList.add('hidden')
  document.getElementById('btn-ftb-submit').disabled = true
  document.getElementById('ftb-modal-backdrop').classList.remove('hidden')
}

function closeFtbModal() {
  document.getElementById('ftb-modal-backdrop').classList.add('hidden')
}

function ftbBackdropClose(e) {
  if (e.target === document.getElementById('ftb-modal-backdrop')) closeFtbModal()
}

async function searchFtbPacks() {
  const term = document.getElementById('ftb-search-input').value.trim()
  if (!term) return

  const btn = document.getElementById('btn-ftb-search')
  const errEl = document.getElementById('ftb-error')
  const resultsEl = document.getElementById('ftb-results')
  btn.disabled = true
  btn.textContent = '…'
  errEl.classList.add('hidden')
  resultsEl.innerHTML = '<div class="ftb-result-item" style="color:var(--text-dim)">Searching…</div>'
  resultsEl.classList.remove('hidden')

  try {
    const data = await fetch(`/api/ftb/search?term=${encodeURIComponent(term)}`)
      .then(r => { if (!r.ok) throw new Error(`Search failed (HTTP ${r.status})`); return r.json() })

    ftbResults = (data.packs || []).filter(p => p && p.id)

    if (ftbResults.length === 0) {
      resultsEl.innerHTML = '<div class="ftb-result-item" style="color:var(--text-dim)">No results found.</div>'
      return
    }

    resultsEl.innerHTML = ftbResults.map((p, i) => `
      <div class="ftb-result-item" onclick="selectFtbPack(${i})">
        <div class="ftb-result-name">${esc(p.name || 'Unknown')}</div>
        <div class="ftb-result-desc">${esc(p.synopsis || '')}</div>
      </div>
    `).join('')
  } catch (err) {
    errEl.textContent = err.message
    errEl.classList.remove('hidden')
    resultsEl.classList.add('hidden')
  } finally {
    btn.disabled = false
    btn.textContent = 'Search'
  }
}

function selectFtbPack(index) {
  ftbSelectedPack = ftbResults[index]

  // Highlight selected
  document.querySelectorAll('.ftb-result-item').forEach((el, i) => {
    el.classList.toggle('selected', i === index)
  })

  // Populate version dropdown
  const versions = (ftbSelectedPack.versions || [])
    .filter(v => v.type === 'Release' || v.type === 'Beta' || v.type === 'Alpha')
    .sort((a, b) => (b.updated || 0) - (a.updated || 0))

  const select = document.getElementById('ftb-version')
  select.innerHTML = versions.map(v =>
    `<option value="${esc(String(v.id))}">${esc(v.name || String(v.id))} [${esc(v.type || '')}]</option>`
  ).join('')

  // Pre-fill instance name
  if (!document.getElementById('ftb-name').value.trim()) {
    document.getElementById('ftb-name').value = ftbSelectedPack.name || ''
    updateFtbDirHint()
  }

  document.getElementById('ftb-pack-name').textContent = ftbSelectedPack.name || ''
  document.getElementById('ftb-pack-desc').textContent = ftbSelectedPack.synopsis || ''
  document.getElementById('ftb-selected').classList.remove('hidden')
  document.getElementById('btn-ftb-submit').disabled = versions.length === 0
}

function updateFtbDirHint() {
  const name = document.getElementById('ftb-name').value.trim()
  const dir  = document.getElementById('ftb-dir').value.trim()
  const hint = document.getElementById('ftb-dir-hint')
  if (dir || !name) {
    hint.textContent = ''
  } else {
    const id = slugify(name)
    hint.textContent = `Default: ~/.local/share/msm/servers/${id}/`
  }
}

async function submitFtb() {
  if (!ftbSelectedPack) return
  const versionId = parseInt(document.getElementById('ftb-version').value, 10)
  const name = document.getElementById('ftb-name').value.trim()
  const dir = document.getElementById('ftb-dir').value.trim()
  const port = parseInt(document.getElementById('ftb-port').value, 10) || 25565

  const errEl = document.getElementById('ftb-error')
  if (!name || !versionId) {
    errEl.textContent = 'Please fill in all required fields.'
    errEl.classList.remove('hidden')
    return
  }

  document.getElementById('ftb-step-1').classList.add('hidden')
  document.getElementById('ftb-step-2').classList.remove('hidden')
  document.getElementById('ftb-progress-msg').textContent = 'Starting import…'

  try {
    await api('POST', '/api/setup/import-ftb', {
      pack_id: ftbSelectedPack.id,
      version_id: versionId,
      server_path: dir,
      instance_name: name,
      port,
    })
    // Progress via SSE — wait for modpack_done / modpack_failed
  } catch (err) {
    appendFtbLog('✗ ' + err.message, true)
    document.getElementById('ftb-progress-msg').textContent = ''
    document.getElementById('btn-ftb-close').classList.remove('hidden')
  }
}

function appendFtbLog(msg, isError = false) {
  const el = document.getElementById('ftb-log-output')
  const line = document.createElement('div')
  line.className = 'setup-log-line' + (isError ? ' setup-log-error' : '')
  line.textContent = msg
  el.appendChild(line)
  el.scrollTop = el.scrollHeight
}

function onFtbDone() {
  document.getElementById('ftb-progress-msg').textContent = 'Import complete!'
  appendFtbLog('✓ Server is ready — find it in the dashboard.')
  document.getElementById('btn-ftb-close').classList.remove('hidden')
}

function onFtbFailed(error) {
  appendFtbLog('✗ ' + error, true)
  document.getElementById('ftb-progress-msg').textContent = ''
  document.getElementById('btn-ftb-close').classList.remove('hidden')
}

// ─── Add Mod modal ────────────────────────────────────────────────────────────

let _addModResults = []
let _addModSelectedHit = null

function openAddModModal() {
  _addModResults = []
  _addModSelectedHit = null
  document.getElementById('add-mod-search-input').value = ''
  document.getElementById('add-mod-results').innerHTML = ''
  document.getElementById('add-mod-results').classList.add('hidden')
  document.getElementById('add-mod-selected').classList.add('hidden')
  document.getElementById('add-mod-error').classList.add('hidden')
  document.getElementById('btn-add-mod-submit').disabled = true
  document.getElementById('add-mod-modal-backdrop').classList.remove('hidden')
  setTimeout(() => document.getElementById('add-mod-search-input').focus(), 50)
}

function closeAddModModal() {
  document.getElementById('add-mod-modal-backdrop').classList.add('hidden')
}

function addModBackdropClose(e) {
  if (e.target === document.getElementById('add-mod-modal-backdrop')) closeAddModModal()
}

async function searchModrinthMods() {
  if (!detailId) return
  const term = document.getElementById('add-mod-search-input').value.trim()
  if (!term) return

  const btn = document.getElementById('btn-add-mod-search')
  const errEl = document.getElementById('add-mod-error')
  const resultsEl = document.getElementById('add-mod-results')
  btn.disabled = true
  btn.textContent = '…'
  errEl.classList.add('hidden')
  document.getElementById('add-mod-selected').classList.add('hidden')
  document.getElementById('btn-add-mod-submit').disabled = true
  resultsEl.innerHTML = '<div class="ftb-result-item" style="color:var(--text-dim)">Searching…</div>'
  resultsEl.classList.remove('hidden')

  try {
    const data = await api('GET', `/api/instances/${detailId}/mods/search?term=${encodeURIComponent(term)}`)
    _addModResults = data ?? []

    if (_addModResults.length === 0) {
      resultsEl.innerHTML = '<div class="ftb-result-item" style="color:var(--text-dim)">No results found.</div>'
      return
    }

    resultsEl.innerHTML = _addModResults.map((hit, i) => {
      const dl = hit.downloads >= 1_000_000
        ? `${(hit.downloads / 1_000_000).toFixed(1)}M`
        : hit.downloads >= 1000
          ? `${Math.round(hit.downloads / 1000)}k`
          : String(hit.downloads)
      return `
        <div class="ftb-result-item" onclick="selectAddModHit(${i})">
          <div class="ftb-result-name">${esc(hit.title)} <span class="mod-dl-count">${dl} downloads</span></div>
          <div class="ftb-result-desc">${esc(hit.description)}</div>
        </div>`
    }).join('')
  } catch (err) {
    errEl.textContent = err.message
    errEl.classList.remove('hidden')
    resultsEl.classList.add('hidden')
  } finally {
    btn.disabled = false
    btn.textContent = 'Search'
  }
}

async function selectAddModHit(index) {
  _addModSelectedHit = _addModResults[index]

  document.querySelectorAll('#add-mod-results .ftb-result-item').forEach((el, i) => {
    el.classList.toggle('selected', i === index)
  })

  const errEl = document.getElementById('add-mod-error')
  errEl.classList.add('hidden')
  document.getElementById('add-mod-title').textContent = _addModSelectedHit.title
  document.getElementById('add-mod-desc').textContent = _addModSelectedHit.description
  document.getElementById('add-mod-selected').classList.remove('hidden')
  document.getElementById('btn-add-mod-submit').disabled = true

  const select = document.getElementById('add-mod-version')
  select.innerHTML = '<option>Loading versions…</option>'

  try {
    const inst = instances.get(detailId)
    const loader = inst?.loader ?? 'neoforge'
    const mcVer = inst?.minecraft_version ?? ''
    const loadersParam = encodeURIComponent(JSON.stringify([loader]))
    const versionsParam = encodeURIComponent(JSON.stringify([mcVer]))

    const versions = await fetch(
      `https://api.modrinth.com/v2/project/${encodeURIComponent(_addModSelectedHit.project_id)}/version?loaders=${loadersParam}&game_versions=${versionsParam}`,
      { headers: { 'User-Agent': 'msm/0.1' } }
    ).then(r => {
      if (!r.ok) throw new Error(`HTTP ${r.status}`)
      return r.json()
    })

    if (!versions.length) {
      select.innerHTML = '<option value="">No compatible versions</option>'
      errEl.textContent = `No versions compatible with MC ${mcVer} + ${loader}`
      errEl.classList.remove('hidden')
      return
    }

    select.innerHTML = versions.map(v =>
      `<option value="${esc(v.id)}">${esc(v.name || v.version_number)} — ${esc(v.version_number)}</option>`
    ).join('')
    document.getElementById('btn-add-mod-submit').disabled = false
  } catch (err) {
    select.innerHTML = ''
    errEl.textContent = 'Failed to load versions: ' + err.message
    errEl.classList.remove('hidden')
  }
}

async function submitAddMod() {
  if (!detailId || !_addModSelectedHit) return
  const versionId = document.getElementById('add-mod-version').value
  if (!versionId) return

  const btn = document.getElementById('btn-add-mod-submit')
  const errEl = document.getElementById('add-mod-error')
  btn.disabled = true
  btn.textContent = 'Adding…'
  errEl.classList.add('hidden')

  try {
    const added = await api('POST', `/api/instances/${detailId}/mods/add`, {
      project_id: _addModSelectedHit.project_id,
      version_id: versionId,
    })
    const mods = await api('GET', `/api/instances/${detailId}/mods`)
    modsData.set(detailId, { mods: mods ?? [], updates: null })
    renderMods()
    closeAddModModal()
    const depCount = (added?.length ?? 1) - 1
    const msg = depCount > 0
      ? `Added ${_addModSelectedHit.title} + ${depCount} dependenc${depCount === 1 ? 'y' : 'ies'}.`
      : `Added ${_addModSelectedHit.title}.`
    setModsMsg(msg, 'success')
  } catch (err) {
    errEl.textContent = 'Failed to add mod: ' + err.message
    errEl.classList.remove('hidden')
    btn.disabled = false
    btn.textContent = 'Add Mod'
  }
}

// ─── Whitelist + Bans drawer ──────────────────────────────────────────────────
let wlEntries  = []
let banPlayers = []
let banIps     = []

function switchWlTab(tab) {
  document.querySelectorAll('.wl-tab').forEach(b => b.classList.toggle('active', b.dataset.wltab === tab))
  document.getElementById('wl-panel-whitelist').classList.toggle('hidden', tab !== 'whitelist')
  document.getElementById('wl-panel-bans').classList.toggle('hidden', tab !== 'bans')
  if (tab === 'bans') loadBans()
}

async function openWhitelist() {
  document.getElementById('wl-overlay').classList.remove('hidden')
  document.getElementById('wl-drawer').classList.remove('hidden')
  switchWlTab('whitelist')
  document.getElementById('wl-input').value = ''
  wlHideMsg()
  await loadWhitelist()
}

function closeWhitelist() {
  document.getElementById('wl-overlay').classList.add('hidden')
  document.getElementById('wl-drawer').classList.add('hidden')
}

async function loadWhitelist() {
  try {
    wlEntries = await api('GET', '/api/whitelist') ?? []
    renderWhitelist()
  } catch (e) {
    wlShowMsg('Failed to load: ' + e.message, 'error')
  }
}

function renderWhitelist() {
  const el = document.getElementById('wl-list')
  if (wlEntries.length === 0) {
    el.innerHTML = '<div class="wl-empty">No players whitelisted yet.</div>'
    return
  }
  el.innerHTML = wlEntries
    .slice()
    .sort((a, b) => a.name.localeCompare(b.name))
    .map(e => `
      <div class="wl-row" id="wl-row-${esc(e.name)}">
        <div class="wl-avatar">${esc(e.name[0].toUpperCase())}</div>
        <div class="wl-info">
          <span class="wl-name">${esc(e.name)}</span>
          <span class="wl-uuid">${esc(e.uuid)}</span>
        </div>
        <button class="wl-remove" onclick="removeFromWhitelist('${esc(e.name)}')" title="Remove">✕</button>
      </div>`).join('')
}

async function addToWhitelist() {
  const input = document.getElementById('wl-input')
  const btn   = document.getElementById('wl-add-btn')
  const username = input.value.trim()
  if (!username) return

  btn.disabled = true
  btn.textContent = '…'
  wlHideMsg()

  try {
    const entry = await api('POST', '/api/whitelist', { username })
    wlEntries.push(entry)
    renderWhitelist()
    input.value = ''
    wlShowMsg(`${entry.name} added and synced to all servers.`, 'success')
  } catch (e) {
    wlShowMsg(e.message, 'error')
  } finally {
    btn.disabled = false
    btn.textContent = 'Add'
    input.focus()
  }
}

async function removeFromWhitelist(name) {
  const row = document.getElementById('wl-row-' + name)
  if (row) row.style.opacity = '0.4'
  wlHideMsg()
  try {
    await api('DELETE', `/api/whitelist/${encodeURIComponent(name)}`)
    wlEntries = wlEntries.filter(e => e.name !== name)
    renderWhitelist()
    wlShowMsg(`${name} removed from all servers.`, 'success')
  } catch (e) {
    if (row) row.style.opacity = ''
    wlShowMsg(e.message, 'error')
  }
}

let _wlMsgTimer = null
function wlShowMsg(text, type) {
  const el = document.getElementById('wl-msg')
  el.textContent = text
  el.className = 'wl-msg' + (type ? ' ' + type : '')
  el.classList.remove('hidden')
  if (_wlMsgTimer) clearTimeout(_wlMsgTimer)
  if (type === 'success') _wlMsgTimer = setTimeout(wlHideMsg, 6000)
}
function wlHideMsg() {
  document.getElementById('wl-msg').classList.add('hidden')
}

// ─── Ban management ───────────────────────────────────────────────────────────
async function loadBans() {
  try {
    banPlayers = await api('GET', '/api/bans/players') ?? []
    banIps     = await api('GET', '/api/bans/ips') ?? []
    renderBanPlayers()
    renderBanIps()
  } catch (e) {
    showBanMsg(e.message, 'error')
  }
}

function renderBanPlayers() {
  const el = document.getElementById('ban-player-list')
  if (!el) return
  if (banPlayers.length === 0) { el.innerHTML = '<div class="wl-empty">No player bans.</div>'; return }
  el.innerHTML = banPlayers
    .slice().sort((a, b) => a.name.localeCompare(b.name))
    .map(p => `<div class="wl-row">
      <div class="wl-avatar ban-avatar">${esc(p.name[0].toUpperCase())}</div>
      <div class="wl-info">
        <span class="wl-name">${esc(p.name)}</span>
        <span class="wl-uuid">${esc(p.reason)}</span>
      </div>
      <button class="wl-remove" onclick="unbanPlayer('${esc(p.name)}')" title="Unban">✕</button>
    </div>`).join('')
}

function renderBanIps() {
  const el = document.getElementById('ban-ip-list')
  if (!el) return
  if (banIps.length === 0) { el.innerHTML = '<div class="wl-empty">No IP bans.</div>'; return }
  el.innerHTML = banIps
    .map(e => `<div class="wl-row">
      <div class="wl-avatar ban-avatar" style="font-size:11px;font-family:var(--mono)">IP</div>
      <div class="wl-info">
        <span class="wl-name">${esc(e.ip)}</span>
        <span class="wl-uuid">${esc(e.reason)}</span>
      </div>
      <button class="wl-remove" onclick="unbanIp('${esc(e.ip)}')" title="Unban">✕</button>
    </div>`).join('')
}

async function banPlayer() {
  const input = document.getElementById('ban-player-input')
  const username = input.value.trim()
  if (!username) return
  showBanMsg('Banning…', '')
  try {
    const entry = await api('POST', '/api/bans/players', { username })
    banPlayers.push(entry)
    renderBanPlayers()
    input.value = ''
    showBanMsg(`${entry.name} banned.`, 'success')
  } catch (e) { showBanMsg(e.message, 'error') }
}

async function unbanPlayer(name) {
  try {
    await api('DELETE', `/api/bans/players/${encodeURIComponent(name)}`)
    banPlayers = banPlayers.filter(p => p.name !== name)
    renderBanPlayers()
    showBanMsg(`${name} unbanned.`, 'success')
  } catch (e) { showBanMsg(e.message, 'error') }
}

async function banIp() {
  const input = document.getElementById('ban-ip-input')
  const ip = input.value.trim()
  if (!ip) return
  showBanMsg('Banning…', '')
  try {
    const entry = await api('POST', '/api/bans/ips', { ip })
    banIps.push(entry)
    renderBanIps()
    input.value = ''
    showBanMsg(`${entry.ip} banned.`, 'success')
  } catch (e) { showBanMsg(e.message, 'error') }
}

async function unbanIp(ip) {
  try {
    await api('DELETE', `/api/bans/ips/${encodeURIComponent(ip)}`)
    banIps = banIps.filter(e => e.ip !== ip)
    renderBanIps()
    showBanMsg(`${ip} unbanned.`, 'success')
  } catch (e) { showBanMsg(e.message, 'error') }
}

let _banMsgTimer = null
function showBanMsg(text, type) {
  const el = document.getElementById('ban-msg')
  if (!el) return
  el.textContent = text
  el.className = 'wl-msg' + (type ? ' ' + type : '')
  el.classList.remove('hidden')
  if (_banMsgTimer) clearTimeout(_banMsgTimer)
  if (type === 'success') _banMsgTimer = setTimeout(() => el.classList.add('hidden'), 5000)
}

// ─── Settings tab ─────────────────────────────────────────────────────────────
const SERVER_PROPS = [
  { key: 'motd',                label: 'Server Description (MOTD)', type: 'text' },
  { key: 'max-players',         label: 'Max Players',              type: 'number', min: 1, max: 1000 },
  { key: 'online-mode',         label: 'Online Mode',              type: 'boolean' },
  { key: 'white-list',          label: 'Enforce Whitelist',        type: 'boolean' },
  { key: 'difficulty',          label: 'Difficulty',               type: 'select', options: ['peaceful','easy','normal','hard'] },
  { key: 'gamemode',            label: 'Default Gamemode',         type: 'select', options: ['survival','creative','adventure','spectator'] },
  { key: 'pvp',                 label: 'PvP',                      type: 'boolean' },
  { key: 'allow-flight',        label: 'Allow Flight',             type: 'boolean' },
  { key: 'allow-nether',        label: 'Allow Nether',             type: 'boolean' },
  { key: 'spawn-protection',    label: 'Spawn Protection Radius',  type: 'number', min: 0, max: 100 },
  { key: 'view-distance',       label: 'View Distance (chunks)',   type: 'number', min: 3, max: 32 },
  { key: 'simulation-distance', label: 'Simulation Distance',      type: 'number', min: 3, max: 32 },
  { key: 'spawn-monsters',      label: 'Spawn Monsters',           type: 'boolean' },
  { key: 'spawn-animals',       label: 'Spawn Animals',            type: 'boolean' },
  { key: 'spawn-npcs',          label: 'Spawn Villagers',          type: 'boolean' },
  { key: 'enable-command-block',label: 'Command Blocks',           type: 'boolean' },
]

let _serverProps = {}

async function loadSettings(id) {
  const form = document.getElementById('props-form')
  if (!form) return
  form.innerHTML = '<div class="settings-loading">Loading…</div>'
  try {
    _serverProps = await api('GET', `/api/instances/${id}/properties`) ?? {}
    renderPropsForm(_serverProps)
    loadRestartConfig(id)
    loadJavaConfig(id)
  } catch (e) {
    form.innerHTML = `<div class="settings-loading">${esc(e.message)}</div>`
  }
}

function renderPropsForm(props) {
  const form = document.getElementById('props-form')
  form.innerHTML = SERVER_PROPS.map(def => {
    const val = props[def.key] ?? ''
    if (def.type === 'boolean') {
      const checked = val === 'true' ? 'checked' : ''
      return `<label class="prop-row prop-row-check">
        <span class="prop-label">${esc(def.label)}</span>
        <input type="checkbox" data-key="${esc(def.key)}" ${checked}>
      </label>`
    }
    if (def.type === 'select') {
      const opts = def.options.map(o => `<option ${o === val ? 'selected' : ''}>${o}</option>`).join('')
      return `<label class="prop-row">
        <span class="prop-label">${esc(def.label)}</span>
        <select class="prop-select" data-key="${esc(def.key)}">${opts}</select>
      </label>`
    }
    const extra = def.min != null ? `min="${def.min}" max="${def.max}"` : ''
    return `<label class="prop-row">
      <span class="prop-label">${esc(def.label)}</span>
      <input type="${def.type}" class="prop-input" data-key="${esc(def.key)}" value="${esc(val)}" ${extra}>
    </label>`
  }).join('')
}

async function saveProperties() {
  if (!detailId) return
  const form = document.getElementById('props-form')
  const updates = {}
  form.querySelectorAll('[data-key]').forEach(el => {
    const key = el.dataset.key
    if (el.type === 'checkbox') updates[key] = el.checked ? 'true' : 'false'
    else updates[key] = el.value
  })
  try {
    await api('POST', `/api/instances/${detailId}/properties`, updates)
    showSettingsMsg('props-msg', 'Properties saved.', 'success')
  } catch (e) {
    showSettingsMsg('props-msg', e.message, 'error')
  }
}

async function loadRestartConfig(id) {
  const inst = instances.get(id)
  if (!inst) return
  // Restart config comes from the instance info if we expose it, else we just use defaults
  // We'll do a GET /api/instances/{id} to get current config
  try {
    const list = await api('GET', '/api/instances')
    const fresh = list?.find(i => i.id === id)
    if (fresh) instances.set(id, fresh)
  } catch { /* silent */ }
}

async function saveRestartConfig() {
  if (!detailId) return
  const body = {
    auto_restart:  document.getElementById('cfg-auto-restart').checked,
    max_attempts:  parseInt(document.getElementById('cfg-max-attempts').value, 10),
    delay_secs:    parseInt(document.getElementById('cfg-delay-secs').value, 10),
    schedule:      document.getElementById('cfg-schedule').value.trim() || null,
    warning_secs:  parseInt(document.getElementById('cfg-warning-secs').value, 10),
  }
  try {
    await api('POST', `/api/instances/${detailId}/restart-config`, body)
    showSettingsMsg('restart-cfg-msg', 'Restart settings saved.', 'success')
  } catch (e) {
    showSettingsMsg('restart-cfg-msg', e.message, 'error')
  }
}

// ─── Java config ──────────────────────────────────────────────────────────────

function recommendedJava(mcVersion) {
  const parts = (mcVersion || '').split('.')
  const minor = parseInt(parts[1] || '21', 10)
  const patch = parseInt(parts[2] || '0', 10)
  if (minor <= 16) return 8
  if (minor <= 19) return 17
  if (minor === 20 && patch < 5) return 17
  return 21
}

async function loadJavaConfig(id) {
  const inst = instances.get(id)
  const currentPath = inst?.java_path ?? null
  const reqJava = recommendedJava(inst?.minecraft_version)

  const noteEl = document.getElementById('java-required-note')
  if (noteEl) noteEl.textContent = `Required: Java ${reqJava}`

  const select = document.getElementById('cfg-java-select')
  if (!select) return

  select.innerHTML = '<option value="">System default</option>'

  try {
    const data = await api('GET', '/api/java/installs')
    const sysVer = data.system_version ? ` (Java ${data.system_version})` : ''
    select.options[0].textContent = `System default${sysVer}`

    for (const install of (data.installs || [])) {
      const opt = document.createElement('option')
      opt.value = install.path
      opt.textContent = `Java ${install.version} — ${install.path}`
      select.appendChild(opt)
    }

    // If current path isn't in the detected list, add it so it's visible
    if (currentPath && ![...select.options].some(o => o.value === currentPath)) {
      const opt = document.createElement('option')
      opt.value = currentPath
      opt.textContent = `Current: ${currentPath}`
      select.appendChild(opt)
    }

    const customOpt = document.createElement('option')
    customOpt.value = '__custom__'
    customOpt.textContent = 'Custom path…'
    select.appendChild(customOpt)

    select.value = currentPath || ''
    if (select.value !== (currentPath || '') && currentPath) {
      select.value = '__custom__'
      document.getElementById('cfg-java-custom').value = currentPath
    }
  } catch {
    // Leave dropdown with just "System default" — non-fatal
  }

  onJavaSelectChange()
}

function onJavaSelectChange() {
  const val = document.getElementById('cfg-java-select')?.value
  const row = document.getElementById('cfg-java-custom-row')
  if (!row) return
  row.classList.toggle('hidden', val !== '__custom__')
}

async function saveJavaConfig() {
  if (!detailId) return
  const select = document.getElementById('cfg-java-select')
  let javaPath = select.value
  if (javaPath === '__custom__') {
    javaPath = document.getElementById('cfg-java-custom').value.trim() || null
  } else if (!javaPath) {
    javaPath = null
  }

  try {
    await api('POST', `/api/instances/${detailId}/java-config`, { java_path: javaPath })
    // Update local cache so the hint stays correct without a full reload
    const inst = instances.get(detailId)
    if (inst) inst.java_path = javaPath
    showSettingsMsg('java-cfg-msg', 'Java settings saved.', 'success')
  } catch (e) {
    showSettingsMsg('java-cfg-msg', e.message, 'error')
  }
}

const _settingsMsgTimers = {}
function showSettingsMsg(elId, text, type) {
  const el = document.getElementById(elId)
  if (!el) return
  el.textContent = text
  el.className = 'settings-msg' + (type ? ' ' + type : '')
  el.classList.remove('hidden')
  if (_settingsMsgTimers[elId]) clearTimeout(_settingsMsgTimers[elId])
  if (type === 'success') _settingsMsgTimers[elId] = setTimeout(() => el.classList.add('hidden'), 5000)
}

// ─── Update version wizard ────────────────────────────────────────────────────
function openUpdateModal() {
  if (!detailId) return
  const inst = instances.get(detailId)
  document.getElementById('update-form').reset()
  document.getElementById('update-error').classList.add('hidden')
  document.getElementById('update-mc-preview').textContent = ''
  document.getElementById('update-step-1').classList.remove('hidden')
  document.getElementById('update-step-2').classList.add('hidden')
  document.getElementById('update-log-output').innerHTML = ''
  document.getElementById('update-progress-msg').textContent = ''
  document.getElementById('btn-update-close').classList.add('hidden')
  document.getElementById('btn-update-submit').disabled = false
  document.getElementById('btn-update-submit').textContent = 'Download & Install'
  const verEl = document.getElementById('update-current-ver')
  verEl.textContent = inst ? `Current: MC ${inst.minecraft_version}` : ''
  document.getElementById('update-modal-backdrop').classList.remove('hidden')
}

function closeUpdateModal() {
  document.getElementById('update-modal-backdrop').classList.add('hidden')
}

function updateBackdropClose(e) {
  if (e.target === e.currentTarget) closeUpdateModal()
}

function updateNfPreview() {
  const ver = document.getElementById('update-nf-ver').value.trim()
  const mc  = neoForgeToMcVersion(ver)
  document.getElementById('update-mc-preview').textContent = mc ? `MC ${mc}` : ''
}

async function submitUpdate(e) {
  e.preventDefault()
  if (!detailId) return
  const btn     = document.getElementById('btn-update-submit')
  const errorEl = document.getElementById('update-error')
  btn.disabled  = true
  btn.textContent = 'Installing…'
  errorEl.classList.add('hidden')

  const nfVer = document.getElementById('update-nf-ver').value.trim()

  document.getElementById('update-step-1').classList.add('hidden')
  document.getElementById('update-step-2').classList.remove('hidden')
  document.getElementById('update-progress-msg').textContent = 'Downloading & installing…'

  try {
    await api('POST', `/api/instances/${detailId}/update-version`, { neoforge_version: nfVer })
  } catch (err) {
    document.getElementById('update-step-1').classList.remove('hidden')
    document.getElementById('update-step-2').classList.add('hidden')
    btn.disabled = false
    btn.textContent = 'Download & Install'
    errorEl.textContent = err.message
    errorEl.classList.remove('hidden')
  }
}

function appendUpdateLog(msg) {
  const el = document.getElementById('update-log-output')
  if (!el) return
  const line = document.createElement('div')
  line.className = 'setup-log-line'
  line.textContent = msg
  el.appendChild(line)
  el.scrollTop = el.scrollHeight
}

function onUpdateDone(instanceId) {
  document.getElementById('update-progress-msg').textContent = 'Update complete!'
  document.getElementById('btn-update-close').classList.remove('hidden')
  appendUpdateLog('✓ Version updated successfully.')
}

function onUpdateFailed(instanceId, error) {
  document.getElementById('update-progress-msg').textContent = ''
  document.getElementById('btn-update-close').classList.remove('hidden')
  const errEl = document.createElement('div')
  errEl.className = 'setup-log-line setup-log-error'
  errEl.textContent = '✗ ' + error
  document.getElementById('update-log-output').appendChild(errEl)
}

// ─── Toasts ───────────────────────────────────────────────────────────────────
function showToast(title, msg, type = 'info', duration = 5000) {
  const container = document.getElementById('toast-container')
  if (!container) return
  const id = ++_toastId
  const icons = { success: '✓', warning: '⚠', error: '✕', info: '◈' }
  const el = document.createElement('div')
  el.className = `toast toast-${type}`
  el.id = `toast-${id}`
  el.innerHTML = `
    <span class="toast-icon">${icons[type] ?? '●'}</span>
    <div class="toast-body">
      <div class="toast-title">${esc(title)}</div>
      ${msg ? `<div class="toast-msg">${esc(msg)}</div>` : ''}
    </div>
    <button class="toast-close" onclick="dismissToast(${id})" type="button">✕</button>
    <div class="toast-progress" style="animation-duration:${duration}ms"></div>`
  container.appendChild(el)
  setTimeout(() => dismissToast(id), duration)
}

function dismissToast(id) {
  const el = document.getElementById('toast-' + id)
  if (!el || el.classList.contains('removing')) return
  el.classList.add('removing')
  setTimeout(() => el.remove(), 200)
}

// ─── Command macros ───────────────────────────────────────────────────────────
function toggleMacros() {
  macrosOpen = !macrosOpen
  document.getElementById('macros-panel')?.classList.toggle('hidden', !macrosOpen)
  document.getElementById('btn-macros')?.classList.toggle('active', macrosOpen)
  if (macrosOpen) renderMacros()
}

function renderMacros() {
  const panel = document.getElementById('macros-panel')
  if (!panel) return
  const listHTML = cmdMacros.length
    ? cmdMacros.map((m, i) => `
        <div class="macro-row" onclick="runMacro(${i})">
          <span class="macro-name">${esc(m.name)}</span>
          <span class="macro-cmd">${esc(m.cmd)}</span>
          <button class="macro-del" onclick="event.stopPropagation();deleteMacro(${i})" type="button" title="Delete">✕</button>
        </div>`).join('')
    : '<div class="macros-empty">No macros yet. Add one below.</div>'
  panel.innerHTML = `
    <div class="macros-panel-header"><span>MACROS</span></div>
    <div class="macros-list">${listHTML}</div>
    <div class="macros-add-row">
      <input id="macro-name-input" type="text" placeholder="Name" autocomplete="off" maxlength="24"
             onkeydown="if(event.key==='Enter')saveMacro()">
      <input id="macro-cmd-input" type="text" placeholder="Command" autocomplete="off"
             onkeydown="if(event.key==='Enter')saveMacro()">
      <button class="btn-add-macro" onclick="saveMacro()" type="button">+</button>
    </div>`
}

function saveMacro() {
  const name = document.getElementById('macro-name-input')?.value.trim()
  const cmd  = document.getElementById('macro-cmd-input')?.value.trim()
  if (!name || !cmd) return
  cmdMacros.push({ name, cmd })
  localStorage.setItem('cmd_macros', JSON.stringify(cmdMacros))
  renderMacros()
}

function deleteMacro(index) {
  cmdMacros.splice(index, 1)
  localStorage.setItem('cmd_macros', JSON.stringify(cmdMacros))
  renderMacros()
}

function runMacro(index) {
  const m = cmdMacros[index]
  if (!m || !detailId) return
  api('POST', `/api/instances/${detailId}/cmd`, { command: m.cmd }).catch(() => {})
}

// ─── Disk usage ───────────────────────────────────────────────────────────────
async function loadDiskUsage(id) {
  if (!id) return
  const el = document.getElementById('disk-usage-content')
  if (!el) return
  el.innerHTML = '<div class="settings-loading">Computing…</div>'
  try {
    const data = await api('GET', `/api/instances/${id}/disk-usage`)
    renderDiskUsage(data)
  } catch (e) {
    el.innerHTML = `<div class="settings-loading">${esc(e.message)}</div>`
  }
}

function renderDiskUsage(data) {
  const el = document.getElementById('disk-usage-content')
  if (!el) return
  const maxBytes = Math.max(data.server_dir_size_bytes, data.backup_size_bytes, 1)
  function row(label, bytes, color) {
    const pct = Math.min(100, (bytes / maxBytes) * 100).toFixed(1)
    return `<div class="disk-row">
      <span class="disk-label">${esc(label)}</span>
      <div class="disk-bar-wrap"><div class="disk-bar" style="width:${pct}%;background:${color}"></div></div>
      <span class="disk-size">${fmtSize(bytes)}</span>
    </div>`
  }
  el.innerHTML = `<div class="disk-usage">
    ${row('World',      data.world_size_bytes,      'var(--green)')}
    ${row('Server dir', data.server_dir_size_bytes, 'var(--blue)')}
    ${row('Backups',    data.backup_size_bytes,     'var(--amber)')}
  </div>`
}

// ─── Stats tab ────────────────────────────────────────────────────────────────
let statsWindow = '24h'
const _charts = {}

function setStatsWindow(w) {
  statsWindow = w
  document.querySelectorAll('.stats-window-btn').forEach(b => b.classList.toggle('active', b.dataset.w === w))
  if (detailId) loadStats(detailId)
}

async function loadStats(id) {
  if (!id) return
  document.getElementById('stats-summary').innerHTML = '<span class="stats-label">Loading…</span>'
  try {
    const [data, players] = await Promise.all([
      api('GET', `/api/instances/${id}/metrics?window=${statsWindow}`),
      api('GET', `/api/instances/${id}/player-stats?window=${statsWindow}`),
    ])
    renderStats(data)
    renderPlayerStats(players)
  } catch (e) {
    document.getElementById('stats-summary').innerHTML = `<span class="stats-label">${esc(e.message)}</span>`
  }
}

function renderStats(data) {
  const { metrics, events, summary } = data

  // Summary chips
  const uptimeCls = summary.uptime_pct >= 90 ? 'good' : summary.uptime_pct >= 60 ? 'warn' : 'bad'
  const tpsCls    = !summary.avg_tps ? '' : summary.avg_tps >= 18 ? 'good' : summary.avg_tps >= 12 ? 'warn' : 'bad'
  const crashCls  = summary.crash_count === 0 ? 'good' : summary.crash_count <= 2 ? 'warn' : 'bad'

  document.getElementById('stats-summary').innerHTML = `
    <div class="stat-chip">
      <span class="stat-chip-label">Uptime</span>
      <span class="stat-chip-value ${uptimeCls}">${summary.uptime_pct.toFixed(1)}%</span>
    </div>
    <div class="stat-chip">
      <span class="stat-chip-label">Avg TPS</span>
      <span class="stat-chip-value ${tpsCls}">${summary.avg_tps != null ? summary.avg_tps.toFixed(1) : '—'}</span>
    </div>
    <div class="stat-chip">
      <span class="stat-chip-label">Avg RAM</span>
      <span class="stat-chip-value">${summary.avg_ram_mb != null ? Math.round(summary.avg_ram_mb) + ' MB' : '—'}</span>
    </div>
    <div class="stat-chip">
      <span class="stat-chip-label">Peak Players</span>
      <span class="stat-chip-value">${summary.peak_players}</span>
    </div>
    <div class="stat-chip">
      <span class="stat-chip-label">Crashes</span>
      <span class="stat-chip-value ${crashCls}">${summary.crash_count}</span>
    </div>`

  if (metrics.length === 0) {
    document.querySelector('.stats-charts').innerHTML = '<div class="stats-no-data">No metrics recorded yet — data appears after the server runs for ~1 minute.</div>'
    document.getElementById('stats-events').innerHTML = ''
    return
  }

  const labels = metrics.map(m => fmtChartTime(m.ts))

  renderChart('chart-tps',     labels, metrics.map(m => m.tps),     'TPS',       '#4ade80', 20)
  renderChart('chart-ram',     labels, metrics.map(m => m.ram_mb),  'RAM (MB)',  '#60a5fa', null)
  renderChart('chart-players', labels, metrics.map(m => m.players), 'Players',   '#f472b6', null)
  renderChart('chart-cpu',     labels, metrics.map(m => m.cpu_pct), 'CPU %',     '#a78bfa', 100)

  // Events list
  const evEl = document.getElementById('stats-events')
  if (events.length === 0) {
    evEl.innerHTML = ''
  } else {
    evEl.innerHTML = events.map(e =>
      `<span class="stats-event-tag ${esc(e.event)}">${fmtChartTime(e.ts)} — ${esc(e.event)}</span>`
    ).join('')
  }
}

function renderChart(canvasId, labels, rawData, label, color, suggestedMax) {
  // Destroy old chart instance if it exists
  if (_charts[canvasId]) { _charts[canvasId].destroy() }

  const canvas = document.getElementById(canvasId)
  if (!canvas) return

  // Replace nulls with NaN so Chart.js draws gaps
  const data = rawData.map(v => (v == null ? NaN : v))

  _charts[canvasId] = new Chart(canvas, {
    type: 'line',
    data: {
      labels,
      datasets: [{
        label,
        data,
        borderColor: color,
        backgroundColor: hexToRgba(color, 0.12),
        borderWidth: 1.5,
        pointRadius: 0,
        pointHoverRadius: 3,
        fill: true,
        tension: 0.3,
        spanGaps: false,
      }]
    },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      animation: false,
      plugins: {
        legend: { display: false },
        tooltip: {
          mode: 'index',
          intersect: false,
          backgroundColor: 'rgba(20,20,28,.9)',
          titleColor: '#94a3b8',
          bodyColor: '#e2e8f0',
          borderColor: '#2d3748',
          borderWidth: 1,
        },
      },
      scales: {
        x: {
          ticks: { color: '#64748b', maxTicksLimit: 8, font: { size: 10 } },
          grid:  { color: 'rgba(255,255,255,.04)' },
        },
        y: {
          min: 0,
          suggestedMax: suggestedMax || undefined,
          ticks: { color: '#64748b', font: { size: 10 } },
          grid:  { color: 'rgba(255,255,255,.04)' },
        },
      },
    }
  })
}

function fmtChartTime(ts) {
  const d = new Date(ts * 1000)
  const h = d.getHours().toString().padStart(2, '0')
  const m = d.getMinutes().toString().padStart(2, '0')
  if (statsWindow === '7d') {
    return `${(d.getMonth()+1)}/${d.getDate()} ${h}:${m}`
  }
  return `${h}:${m}`
}

function hexToRgba(hex, alpha) {
  const r = parseInt(hex.slice(1, 3), 16)
  const g = parseInt(hex.slice(3, 5), 16)
  const b = parseInt(hex.slice(5, 7), 16)
  return `rgba(${r},${g},${b},${alpha})`
}

function renderPlayerStats(data) {
  const el = document.getElementById('stats-players')
  if (!el) return
  if (!data || data.stats.length === 0) { el.innerHTML = ''; return }

  function fmtDur(secs) {
    if (!secs) return '—'
    const h = Math.floor(secs / 3600)
    const m = Math.floor((secs % 3600) / 60)
    return h > 0 ? `${h}h ${m}m` : `${m}m`
  }
  function fmtTs(ts) {
    return new Date(ts * 1000).toLocaleDateString()
  }

  const rows = data.stats.map(s => `
    <tr>
      <td class="stats-player-name">${esc(s.player)}</td>
      <td>${s.sessions}</td>
      <td>${fmtDur(s.total_secs)}</td>
      <td>${fmtTs(s.last_seen)}</td>
    </tr>`).join('')

  el.innerHTML = `
    <div class="stats-players-header">Player Activity</div>
    <table class="stats-player-table">
      <thead><tr><th>Player</th><th>Sessions</th><th>Total Time</th><th>Last Seen</th></tr></thead>
      <tbody>${rows}</tbody>
    </table>`
}

// ─── Alerts config ────────────────────────────────────────────────────────────

async function loadBackupConfig(id) {
  if (!id) return
  try {
    const data = await api('GET', `/api/instances/${id}/backup-config`)
    document.getElementById('backup-enabled').checked    = !!data.enabled
    document.getElementById('backup-schedule').value     = data.schedule ?? ''
    document.getElementById('backup-keep').value         = data.keep_count ?? 10
    document.getElementById('backup-world-only').checked = !!data.world_only
  } catch { /* silent */ }
}

async function saveBackupConfig() {
  if (!detailId) return
  const body = {
    enabled:    document.getElementById('backup-enabled').checked,
    schedule:   document.getElementById('backup-schedule').value.trim() || null,
    keep_count: parseInt(document.getElementById('backup-keep').value) || 10,
    world_only: document.getElementById('backup-world-only').checked,
  }
  const msg = document.getElementById('backup-cfg-msg')
  try {
    await api('POST', `/api/instances/${detailId}/backup-config`, body)
    msg.textContent = 'Saved'
    msg.style.color = 'var(--green)'
    msg.classList.remove('hidden')
    setTimeout(() => msg.classList.add('hidden'), 2000)
  } catch {
    msg.textContent = 'Failed to save'
    msg.style.color = 'var(--red)'
    msg.classList.remove('hidden')
  }
}

async function loadAlertsConfig(id) {
  if (!id) return
  try {
    const data = await api('GET', `/api/instances/${id}/alerts-config`)
    document.getElementById('alert-enabled').checked = !!data.enabled
    document.getElementById('alert-tps-min').value = data.tps_min ?? 15
    document.getElementById('alert-tps-consecutive').value = data.tps_consecutive ?? 3
    document.getElementById('alert-ram-pct').value = data.ram_pct_max ?? 90
    document.getElementById('alert-max-ram').value = data.max_ram_mb ?? 0
  } catch { /* silent */ }
}

async function saveAlertsConfig() {
  if (!detailId) return
  const body = {
    enabled:          document.getElementById('alert-enabled').checked,
    tps_min:          parseFloat(document.getElementById('alert-tps-min').value),
    tps_consecutive:  parseInt(document.getElementById('alert-tps-consecutive').value, 10),
    ram_pct_max:      parseInt(document.getElementById('alert-ram-pct').value, 10),
    max_ram_mb:       parseInt(document.getElementById('alert-max-ram').value, 10) || 0,
  }
  try {
    await api('POST', `/api/instances/${detailId}/alerts-config`, body)
    showSettingsMsg('alerts-cfg-msg', 'Alert settings saved.', 'success')
  } catch (e) {
    showSettingsMsg('alerts-cfg-msg', e.message, 'error')
  }
}

// ─── Scheduled commands ───────────────────────────────────────────────────────

async function loadSchedules(id) {
  if (!id) return
  try {
    const data = await api('GET', `/api/instances/${id}/schedules`)
    renderSchedules(data)
  } catch { /* silent */ }
}

function renderSchedules(list) {
  const el = document.getElementById('schedules-list')
  if (!el) return
  if (!list || list.length === 0) { el.innerHTML = ''; return }
  el.innerHTML = list.map(s => `
    <div class="schedule-item">
      <span class="schedule-item-name">${esc(s.name)}</span>
      <span class="schedule-item-cmd">${esc(s.command)}</span>
      <span class="schedule-item-interval">every ${fmtInterval(s.interval_secs)}</span>
      <label class="schedule-item-toggle" title="Enable/disable">
        <input type="checkbox" ${s.enabled ? 'checked' : ''}
          onchange="toggleSchedule(${JSON.stringify(s.name)}, this.checked, ${s.interval_secs}, ${JSON.stringify(s.command)})">
      </label>
      <button class="schedule-item-del" onclick="deleteSchedule(${JSON.stringify(s.name)})" title="Delete">✕</button>
    </div>`).join('')
}

function fmtInterval(secs) {
  if (secs < 60) return `${secs}s`
  if (secs < 3600) return `${Math.round(secs / 60)}m`
  return `${Math.round(secs / 3600)}h`
}

async function addSchedule() {
  if (!detailId) return
  const name     = document.getElementById('sched-name').value.trim()
  const cmd      = document.getElementById('sched-cmd').value.trim()
  const interval = parseInt(document.getElementById('sched-interval').value, 10)
  if (!name || !cmd || !interval || interval < 10) {
    showSettingsMsg('schedules-msg', 'Name, command and interval (≥10s) are required.', 'error')
    return
  }
  try {
    await api('POST', `/api/instances/${detailId}/schedules`, { name, command: cmd, interval_secs: interval, enabled: true })
    document.getElementById('sched-name').value = ''
    document.getElementById('sched-cmd').value = ''
    document.getElementById('sched-interval').value = ''
    await loadSchedules(detailId)
    showSettingsMsg('schedules-msg', 'Schedule added.', 'success')
  } catch (e) {
    showSettingsMsg('schedules-msg', e.message, 'error')
  }
}

async function toggleSchedule(name, enabled, interval_secs, command) {
  if (!detailId) return
  try {
    await api('POST', `/api/instances/${detailId}/schedules`, { name, command, interval_secs, enabled })
  } catch (e) {
    showSettingsMsg('schedules-msg', e.message, 'error')
    loadSchedules(detailId)
  }
}

async function deleteSchedule(name) {
  if (!detailId) return
  try {
    await api('DELETE', `/api/instances/${detailId}/schedules/${encodeURIComponent(name)}`)
    await loadSchedules(detailId)
    showSettingsMsg('schedules-msg', 'Schedule deleted.', 'success')
  } catch (e) {
    showSettingsMsg('schedules-msg', e.message, 'error')
  }
}

// ─── Action helpers ───────────────────────────────────────────────────────────
async function doStart(id) {
  try {
    await api('POST', `/api/instances/${id}/start`)
  } catch (err) {
    showCardError(id, err.message)
    if (detailId === id) setDetailError(err.message)
  }
}

async function doStop(id) {
  const inst = instances.get(id)
  if (inst?.players?.length) {
    const names = inst.players.join(', ')
    if (!confirm(`${inst.players.length} player(s) currently online: ${names}\n\nStop the server anyway?`)) return
  }
  try {
    await api('POST', `/api/instances/${id}/stop`)
  } catch (err) {
    showCardError(id, err.message)
    if (detailId === id) setDetailError(err.message)
  }
}

async function doSwitch(id) {
  const running = [...instances.values()].find(i => i.status === 'running' || i.status === 'starting')
  if (running?.players?.length) {
    const names = running.players.join(', ')
    if (!confirm(`${running.players.length} player(s) online on "${running.display_name || running.id}": ${names}\n\nSwitch server anyway?`)) return
  }
  try {
    await api('POST', `/api/instances/${id}/switch`)
  } catch (err) {
    showCardError(id, err.message)
  }
}

function showCardError(id, msg) {
  const el = document.getElementById('card-error-' + id)
  if (!el) return
  el.textContent = msg
  el.classList.remove('hidden')
  setTimeout(() => el.classList.add('hidden'), 5000)
}

// ─── Utilities ────────────────────────────────────────────────────────────────
async function api(method, path, body) {
  const opts = { method, headers: {} }
  if (body) { opts.headers['Content-Type'] = 'application/json'; opts.body = JSON.stringify(body) }
  const res = await fetch(path, opts)
  if (res.status === 204) return
  const json = await res.json().catch(() => ({ error: res.statusText }))
  if (!res.ok) throw new Error(json.error ?? res.statusText)
  return json
}

function statusLabel(s) {
  return { running: 'RUNNING', stopped: 'STOPPED', starting: 'STARTING', stopping: 'STOPPING', crashed: 'CRASHED' }[s] ?? s.toUpperCase()
}

function logLevel(line) {
  if (/\/(ERROR|FATAL)\]/.test(line)) return 'log-error'
  if (/\/WARN\]/.test(line))          return 'log-warn'
  if (/\/DEBUG\]/.test(line))         return 'log-debug'
  return 'log-info'
}

function fmtTime(ts) {
  return new Date(ts * 1000).toLocaleTimeString('en-GB', { hour12: false })
}

function esc(str) {
  return String(str).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;')
}

function slugify(s) {
  return s.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '')
}

function fmtSize(bytes) {
  if (bytes >= 1_073_741_824) return (bytes / 1_073_741_824).toFixed(1) + ' GB'
  if (bytes >= 1_048_576)     return (bytes / 1_048_576).toFixed(1) + ' MB'
  if (bytes >= 1_024)         return Math.round(bytes / 1_024) + ' KB'
  return bytes + ' B'
}

function fmtDate(ts) {
  return new Date(ts * 1000).toLocaleString('en-GB', { dateStyle: 'medium', timeStyle: 'short' })
}

// ─── Discord notification toggles ────────────────────────────────────────────

async function openDiscordNotifyModal() {
  const res = await fetch('/api/discord-notify')
  if (!res.ok) {
    showToast('Discord is not configured in config.toml', 'error')
    return
  }
  const cfg = await res.json()
  document.getElementById('dn-started').checked      = cfg.server_started
  document.getElementById('dn-stopped').checked      = cfg.server_stopped
  document.getElementById('dn-crashed').checked      = cfg.server_crashed
  document.getElementById('dn-backup-done').checked  = cfg.backup_done
  document.getElementById('dn-backup-failed').checked = cfg.backup_failed
  document.getElementById('dn-health-alerts').checked = cfg.health_alerts
  document.getElementById('discord-notify-msg').classList.add('hidden')
  document.getElementById('discord-notify-modal').classList.remove('hidden')
}

function closeDiscordNotifyModal() {
  document.getElementById('discord-notify-modal').classList.add('hidden')
}

function discordNotifyBackdropClose(e) {
  if (e.target === document.getElementById('discord-notify-modal')) closeDiscordNotifyModal()
}

async function saveDiscordNotify() {
  const body = {
    server_started:  document.getElementById('dn-started').checked,
    server_stopped:  document.getElementById('dn-stopped').checked,
    server_crashed:  document.getElementById('dn-crashed').checked,
    backup_done:     document.getElementById('dn-backup-done').checked,
    backup_failed:   document.getElementById('dn-backup-failed').checked,
    health_alerts:   document.getElementById('dn-health-alerts').checked,
  }
  const res = await fetch('/api/discord-notify', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  if (res.ok) {
    closeDiscordNotifyModal()
    showToast('Discord notification settings saved')
  } else {
    const msg = document.getElementById('discord-notify-msg')
    msg.textContent = 'Failed to save'
    msg.classList.remove('hidden')
  }
}
