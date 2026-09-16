/* omniroute-rust dashboard — vanilla JS, no build step */
const $ = (id) => document.getElementById(id);
let modelsCache = [];

async function api(path, opts) {
  const r = await fetch(path, opts);
  if (!r.ok) throw new Error(`${path}: HTTP ${r.status}`);
  return r.json();
}

function statusBadge(s) {
  const ok = s >= 200 && s < 400;
  return `<span class="${ok ? 's-ok' : 's-err'}">${s}</span>`;
}

function esc(s) {
  return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

function fmtKb(kb) {
  return kb >= 1048576 ? `${(kb / 1048576).toFixed(1)} GB` : `${Math.round(kb / 1024)} MB`;
}

function fmtUptime(s) {
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${Math.floor(s % 60)}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

// ── tabs ──
document.querySelectorAll('#tabs button').forEach((b) => {
  b.addEventListener('click', () => {
    document.querySelectorAll('#tabs button').forEach((x) => x.classList.remove('active'));
    document.querySelectorAll('.tab').forEach((x) => x.classList.remove('active'));
    b.classList.add('active');
    $(`tab-${b.dataset.tab}`).classList.add('active');
    refreshTab(b.dataset.tab);
  });
});

// ── health ──
async function pollHealth() {
  try {
    const r = await fetch('/healthz');
    const ok = r.ok;
    $('health-dot').className = 'dot' + (ok ? ' ok' : '');
    $('health-text').textContent = ok ? 'healthy' : `HTTP ${r.status}`;
  } catch {
    $('health-dot').className = 'dot';
    $('health-text').textContent = 'unreachable';
  }
}

// ── overview ──
async function renderOverview() {
  const [providers, stats, logs] = await Promise.all([
    api('/v1/providers').catch(() => null),
    api('/v1/stats').catch(() => null),
    api('/v1/logs?limit=10').catch(() => null),
  ]);
  const cards = [];
  if (stats) {
    cards.push([fmtUptime(stats.uptime_s), 'uptime']);
    cards.push([stats.requests, 'requests']);
    cards.push([stats.failures, 'failures', stats.failures > 0 ? 'n s-err' : 'n']);
    cards.push([stats.providers_with_keys, 'providers configured']);
    cards.push([stats.models, 'models']);
    if (stats.memory_kb > 0) cards.push([fmtKb(stats.memory_kb), 'gateway RSS']);
  }
  if (providers) {
    const okCount = providers.providers.filter((p) => p.cooldownMs === 0).length;
    cards.push([`${okCount}/${providers.providers.length}`, 'providers healthy']);
  }
  $('cards').innerHTML = cards
    .map(([n, l]) => `<div class="card"><div class="n">${n}</div><div class="l">${l}</div></div>`)
    .join('');
  fillLogRows($('ov-logs').tBodies[0], logs);
  $('meta').textContent = stats ? `pid ${stats.pid}` : '';
}

// ── providers ──
async function renderProviders() {
  const v = await api('/v1/providers').catch(() => null);
  if (!v) return;
  $('provider-rows').innerHTML = v.providers
    .map((p) =>
      `<tr><td>${esc(p.id)}</td><td>${esc(p.format)}</td><td>${esc(p.baseUrl)}</td><td>${p.isLocal}</td><td>${p.hasKey}</td><td>${p.inFlight}</td><td>${p.cooldownMs > 0 ? `<span class="s-err">${p.cooldownMs}ms</span>` : '<span class="s-ok">0</span>'}</td></tr>`)
    .join('');
}

// ── models ──
async function renderModels() {
  const v = await api('/v1/models');
  modelsCache = v.data || [];
  drawModels();
}
function drawModels() {
  const q = ($('model-search').value || '').toLowerCase();
  $('model-rows').innerHTML = modelsCache
    .filter((m) => !q || m.id.toLowerCase().includes(q))
    .slice(0, 300)
    .map((m) => `<tr><td>${esc(m.id)}</td><td>${esc(m.provider)}</td><td>${m.contextLength}</td></tr>`)
    .join('');
}
$('model-search').addEventListener('input', drawModels);
$('logout').style.display = localStorage.getItem('omniroute_session') ? 'inline' : 'none';

// ── combos ──
async function renderCombos() {
  const v = await api('/v1/combos');
  $('combo-list').innerHTML = v.combos.length
    ? v.combos
        .map((c) => `<div class="combo"><b>${esc(c.name)}</b> <span class="badge">${esc(c.strategy)}</span><div class="chain">${c.providers.map(esc).join(' → ')}</div></div>`)
        .join('')
    : '<div class="combo">no combos configured — define [[combos]] in omniroute.toml</div>';
}

// ── compression ──
async function renderCompression() {
  const c = await api('/v1/compression');
  const modes = c.modes;
  $('compression-form').innerHTML = `
    <div class="comp-card">
      <h2 style="margin-top:0">Runtime compression settings</h2>
      <label>enabled</label>
      <select id="c-enabled"><option value="true">true</option><option value="false">false</option></select>
      <label>default_mode</label>
      <select id="c-mode">${modes.map((m) => `<option ${m === c.default_mode ? 'selected' : ''}>${m}</option>`).join('')}</select>
      <label>auto_trigger_tokens (0 = off)</label>
      <input type="number" id="c-auto" value="${c.auto_trigger_tokens}">
      <label>caveman_intensity (standard mode)</label>
      <select id="c-intensity">${['lite', 'full', 'ultra'].map((m) => `<option ${m === c.caveman_intensity ? 'selected' : ''}>${m}</option>`).join('')}</select>
      <label>preserve_system_prompt</label>
      <select id="c-sys"><option value="true" ${c.preserve_system_prompt ? 'selected' : ''}>true</option><option value="false" ${!c.preserve_system_prompt ? 'selected' : ''}>false</option></select>
      <label>min_message_length</label>
      <input type="number" id="c-minlen" value="${c.min_message_length}">
      <label>ultra_compression_rate</label>
      <input type="number" step="0.05" id="c-rate" value="${c.ultra_compression_rate}">
      <label>rtk_max_lines</label>
      <input type="number" id="c-rtk" value="${c.rtk_max_lines}">
      <div><button class="save" id="c-save">Save</button>
      <span style="color:#8a93b5;font-size:12px">per-request override: x-omniroute-compression header</span></div>
    </div>`;
  $('c-enabled').value = String(c.enabled);
  $('c-sys').value = String(c.preserve_system_prompt);
  $('c-save').addEventListener('click', async () => {
    const body = {
      enabled: $('c-enabled').value === 'true',
      default_mode: $('c-mode').value,
      auto_trigger_tokens: Number($('c-auto').value) || 0,
      caveman_intensity: $('c-intensity').value,
      preserve_system_prompt: $('c-sys').value === 'true',
      min_message_length: Number($('c-minlen').value) || 50,
      ultra_compression_rate: Number($('c-rate').value),
      rtk_max_lines: Number($('c-rtk').value) || 200,
    };
    await api('/v1/compression', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    renderCompression();
  });
}

// ── logs ──
function logRows(logs) {
  return (logs.logs || [])
    .map(
      (l) =>
        `<tr><td>${new Date(l.ts_ms).toLocaleTimeString()}</td><td>${esc(l.model)}</td><td>${esc(l.provider || '-')}</td><td>${statusBadge(l.status)}</td><td>${l.latency_ms}</td><td>${l.tokens_saved || 0}</td></tr>`
    )
    .join('');
}

async function renderLogs() {
  const logs = await api('/v1/logs?limit=100');
  fillLogRows($('log-rows').tBodies[0], logs);
}

function fillLogRows(tbody, logs) {
  if (!logs || !tbody) return;
  tbody.innerHTML = logRows(logs);
}

const renderers = {
  overview: renderOverview,
  providers: renderProviders,
  models: renderModels,
  combos: renderCombos,
  compression: renderCompression,
  logs: renderLogs,
};
function refreshTab(tab) {
  (renderers[tab] || (() => {}))().catch(() => {});
}

// boot
(async () => {
  try {
    const h = await api('/api/health');
    $('version').textContent = 'v' + (h.version || '?');
  } catch {}
  pollHealth();
  setInterval(pollHealth, 5000);
  renderOverview();
  setInterval(() => {
    const active = document.querySelector('#tabs button.active');
    if (!active) return;
    if (active.dataset.tab === 'overview' || active.dataset.tab === 'logs') refreshTab(active.dataset.tab);
  }, 5000);
  // PWA service worker (http(s) only)
  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/dashboard/sw.js').catch(() => {});
  }
})();
