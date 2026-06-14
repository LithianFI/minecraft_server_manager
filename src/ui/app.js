// ─── State ───────────────────────────────────────────────────────────────────
const instances = new Map()   // id → InstanceInfo
const logs      = new Map()   // id → [{line, timestamp}]
let   detailId  = null
let   logPinned = true

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
      updateCard(inst)
      if (detailId === ev.instance_id) refreshDetail(inst)
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
  }
}

// ─── Navigation ───────────────────────────────────────────────────────────────
function showDetail(id) {
  const inst = instances.get(id)
  if (!inst) return
  detailId = id
  logPinned = true
  document.getElementById('view-dashboard').classList.add('hidden')
  const view = document.getElementById('view-detail')
  view.classList.remove('hidden')
  refreshDetail(inst)
  switchTab('logs')
  renderLogs()
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
  if (inst.players.length === 0) return `<div class="card-players dim"><span class="icon">◈</span> No players online</div>`
  const names = inst.players.slice(0, 3).map(esc).join(', ') + (inst.players.length > 3 ? '…' : '')
  return `<div class="card-players"><span class="icon">◈</span> ${inst.players.length} online <span class="card-player-names">${names}</span></div>`
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
}

function refreshPlayerBar(inst) {
  const bar = document.getElementById('player-bar')
  if (!inst || inst.players.length === 0) { bar.classList.add('hidden'); return }
  bar.classList.remove('hidden')
  bar.innerHTML = `<span class="label">Online:</span>` + inst.players.map(p => `<span class="player-tag">${esc(p)}</span>`).join('')
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
}

// ─── Log view ─────────────────────────────────────────────────────────────────
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

  const div = document.createElement('div')
  div.className = 'log-line ' + logLevel(line)
  div.innerHTML = `<span class="log-ts">${fmtTime(timestamp)}</span><span class="log-msg">${esc(line)}</span>`
  el.appendChild(div)

  // Prune oldest lines from DOM to cap at 1000
  while (el.children.length > 1000) el.removeChild(el.firstChild)

  if (autoScroll && logPinned) el.scrollTop = el.scrollHeight
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

  const data = {
    id:                slugify(document.getElementById('add-name').value),
    display_name:      document.getElementById('add-name').value.trim(),
    server_path:       document.getElementById('add-path').value.trim(),
    minecraft_version: document.getElementById('add-ver').value.trim(),
    port:              parseInt(document.getElementById('add-port').value, 10),
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
