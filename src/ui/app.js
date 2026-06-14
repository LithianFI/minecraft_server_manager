// ─── State ───────────────────────────────────────────────────────────────────
const instances  = new Map()   // id → InstanceInfo
const logs       = new Map()   // id → [{line, timestamp}]
const backups    = new Map()   // id → [BackupInfo]
const modsData   = new Map()   // id → { mods: [], updates: null }
let   detailId   = null
let   logPinned  = true
let   logFilter  = 'all'
let   logSearch  = ''

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
      inst.status = ev.status
      if (ev.status !== 'running' && ev.status !== 'starting') {
        inst.ram_mb = null
        inst.tps    = null
      }
      updateCard(inst)
      if (detailId === ev.instance_id) {
        refreshDetail(inst)
        refreshMetricsBar(inst)
      }
      break
    }

    case 'player_joined': {
      const inst = instances.get(ev.instance_id)
      if (!inst || inst.players.includes(ev.player)) break
      inst.players.push(ev.player)
      updateCard(inst)
      if (detailId === ev.instance_id) refreshPlayerBar(inst)
      break
    }

    case 'player_left': {
      const inst = instances.get(ev.instance_id)
      if (!inst) break
      inst.players = inst.players.filter(p => p !== ev.player)
      updateCard(inst)
      if (detailId === ev.instance_id) refreshPlayerBar(inst)
      break
    }

    case 'backup_done': {
      setBackupMsg(`Backup created (${fmtSize(ev.size_bytes)})`, 'success')
      document.getElementById('btn-create-backup').disabled = false
      if (ev.instance_id === detailId) loadBackups(detailId)
      break
    }

    case 'backup_failed': {
      setBackupMsg(`Backup failed: ${ev.error}`, 'error')
      document.getElementById('btn-create-backup').disabled = false
      break
    }

    case 'metrics': {
      const inst = instances.get(ev.instance_id)
      if (!inst) break
      inst.ram_mb = ev.ram_mb
      if (ev.tps != null) inst.tps = ev.tps
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
  const searchEl = document.getElementById('log-search')
  if (searchEl) searchEl.value = ''
  document.querySelectorAll('.log-filter-btn').forEach(b => b.classList.toggle('active', b.dataset.level === 'all'))
  document.getElementById('view-dashboard').classList.add('hidden')
  document.getElementById('view-detail').classList.remove('hidden')
  refreshDetail(inst)
  switchTab('logs')
  renderLogs()
  loadBackups(id)
  loadMods(id)
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
    return
  }
  empty.classList.add('hidden')
  grid.innerHTML = arr.map(i => cardHTML(i, running)).join('')
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

  let playerLine
  if (inst.players.length === 0) {
    playerLine = `<div class="card-players dim"><span class="icon">◈</span> No players online</div>`
  } else {
    const names = inst.players.slice(0, 3).map(esc).join(', ') + (inst.players.length > 3 ? '…' : '')
    playerLine = `<div class="card-players"><span class="icon">◈</span> ${inst.players.length} online <span class="card-player-names">${names}</span></div>`
  }

  let metricsLine = ''
  if (inst.ram_mb != null) {
    const ram = inst.ram_mb >= 1024 ? `${(inst.ram_mb / 1024).toFixed(1)} GB` : `${inst.ram_mb} MB`
    const tps = inst.tps != null ? ` · <span class="card-tps ${tpsClass(inst.tps)}">${inst.tps.toFixed(1)} TPS</span>` : ''
    metricsLine = `<div class="card-metrics">${ram} RAM${tps}</div>`
  }

  return playerLine + metricsLine
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

function refreshPlayerBar(inst) {
  const bar = document.getElementById('player-bar')
  if (!inst || inst.players.length === 0) { bar.classList.add('hidden'); return }
  bar.classList.remove('hidden')
  bar.innerHTML = `<span class="label">Online:</span>` + inst.players.map(p => `<span class="player-tag">${esc(p)}</span>`).join('')
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
  if (name === 'backups'  && detailId) renderBackups()
  if (name === 'mods'     && detailId) renderMods()
  if (name === 'settings' && detailId) loadSettings(detailId)
}

// ─── Log view ─────────────────────────────────────────────────────────────────
function linePassesFilter(line) {
  if (logFilter !== 'all') {
    const level = logLevel(line)
    if (logFilter === 'error' && level !== 'log-error') return false
    if (logFilter === 'warn'  && level !== 'log-warn' && level !== 'log-error') return false
    if (logFilter === 'info'  && level === 'log-debug') return false
  }
  if (logSearch && !line.toLowerCase().includes(logSearch.toLowerCase())) return false
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

document.addEventListener('DOMContentLoaded', () => {
  const logEl = document.getElementById('log-output')
  if (logEl) {
    logEl.addEventListener('scroll', () => {
      const { scrollTop, scrollHeight, clientHeight } = logEl
      logPinned = scrollHeight - scrollTop - clientHeight < 40
    })
  }
})

function submitCmd(e) {
  e.preventDefault()
  const input = document.getElementById('cmd-input')
  const cmd = input.value.trim()
  if (!cmd || !detailId) return
  input.value = ''
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
  return `<div class="backup-row">
    <div class="backup-info">
      <span class="backup-filename">${name}</span>
      <span class="backup-meta">${fmtSize(b.size_bytes)} &middot; ${fmtDate(b.created_at)}</span>
    </div>
    <button class="btn-outline btn-restore" onclick="doRestore('${name}')">Restore</button>
  </div>`
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
  try {
    await api('POST', `/api/instances/${id}/stop`)
  } catch (err) {
    showCardError(id, err.message)
    if (detailId === id) setDetailError(err.message)
  }
}

async function doSwitch(id) {
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
