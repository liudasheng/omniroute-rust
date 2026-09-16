/* omniroute-rust dashboard — sidebar mirroring the original
 * src/shared/constants/sidebarVisibility/sections.ts (v3.8.x). */
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

// ── i18n: original messages pack lookup ──
let PACK = null;
async function loadPack(code) {
  PACK = null;
  try {
    const r = await fetch('/dashboard/locales/' + code + '.json');
    if (r.ok) PACK = await r.json();
  } catch {}
}
function T(key) {
  let cur = PACK;
  for (const seg of key.split('.')) {
    cur = cur && cur[seg];
    if (!cur) return null;
  }
  return typeof cur === 'string' ? cur : null;
}
const label = (k, fallback) => T('sidebar.' + k) || fallback;
const subLabel = (k, fallback) => (k ? T('sidebar.' + k + 'Subtitle') || fallback : fallback);

async function api(path, opts = {}) {
  const token = localStorage.getItem('omniroute_session');
  const headers = Object.assign({}, opts.headers || {});
  if (token) headers.authorization = 'Bearer ' + token;
  const r = await fetch(path, { ...opts, headers });
  if (r.status === 401) {
    const me = await fetch('/v1/auth/me').then((x) => x.json());
    if (!me.authenticated) { showLogin(true); throw new Error(path + ': 401'); }
    const retry = await fetch(path, { ...opts, headers });
    if (!retry.ok) throw new Error(path + ': HTTP ' + retry.status);
    return retry.json();
  }
  if (!r.ok) throw new Error(path + ': HTTP ' + r.status);
  return r.json();
}


// ── icon accents: port of getDeterministicIconAccent (sidebarVisibility.ts) ──
function iconAccent(itemId) {
  let hash = 0;
  for (let i = 0; i < itemId.length; i += 1) hash = (hash * 31 + itemId.charCodeAt(i)) >>> 0;
  const hue = hash % 360;
  const saturation = 72;
  const lightness = 56;
  const chroma = (1 - Math.abs((2 * lightness) / 100 - 1)) * (saturation / 100);
  const huePrime = hue / 60;
  const x = chroma * (1 - Math.abs((huePrime % 2) - 1));
  const match = lightness / 100 - chroma / 2;
  const [red, green, blue] =
    huePrime < 1 ? [chroma, x, 0]
    : huePrime < 2 ? [x, chroma, 0]
    : huePrime < 3 ? [0, chroma, x]
    : huePrime < 4 ? [0, x, chroma]
    : huePrime < 5 ? [x, 0, chroma]
    : [chroma, 0, x];
  return '#' + [red, green, blue]
    .map((c) => Math.round((c + match) * 255).toString(16).padStart(2, '0').toUpperCase())
    .join('');
}

// ── state ──
let modelsCache = [];

// ── sidebar tree: mirroring src/shared/constants/sidebarVisibility/sections.ts ──
const NAV = [
  { hideTitle: true, items: [{ id: 'home', p: 'home', k: 'home', icon: 'home', label: 'Home', sub: 'Gateway status' }] },
  { title: 'OmniProxy', k: 'omniProxySection', items: [
    { id: 'endpoints', p: 'endpoints', k: 'endpoints', icon: 'api', label: 'Endpoints', sub: 'Served AI surface' },
    { id: 'api-manager', p: 'apikeys', k: 'apiManager', icon: 'vpn_key', label: 'API Manager', sub: 'Client API keys' },
    { id: 'providers', p: 'providers', k: 'providers', icon: 'dns', label: 'Providers', sub: 'Connections & catalog' },
    { id: 'combos', p: 'combos', k: 'combos', icon: 'layers', label: 'Combos', sub: 'Routing chains' },
    { id: 'quota', p: 'quota', k: 'providerQuota', icon: 'tune', label: 'Provider Quota', sub: 'Rate-limit state' },
    { title: 'Compression Context', k: 'contextGroup', grp: true },
    { id: 'context-settings', p: 'compression', k: 'contextSettings', icon: 'settings', label: 'Compression Settings', sub: 'Global defaults' },
    { id: 'context-caveman', p: 'compression', k: 'contextCaveman', icon: 'compress', label: 'Caveman', sub: 'Rule engine' },
    { id: 'context-rtk', p: 'compression', k: 'contextRtk', icon: 'filter_alt', label: 'RTK', sub: 'Output filters' },
    { id: 'context-ultra', p: 'compression', k: 'contextUltra', icon: 'bolt', label: 'Ultra', sub: 'Heuristic pruning' },
    { id: 'context-aggressive', p: 'compression', k: 'contextAggressive', icon: 'speed', label: 'Aggressive', sub: 'Summary + aging' },
    { id: 'context-lite', p: 'compression', k: 'contextLite', icon: 'compress', label: 'Lite', sub: 'Whitespace cleanup' },
  ]},
  { title: 'Analytics', k: 'analyticsSection', items: [
    { id: 'usage', p: 'usage', k: 'usage', icon: 'analytics', label: 'Usage', sub: 'Request analytics' },
    { id: 'provider-stats', p: 'providers', k: 'providerStats', icon: 'speed', label: 'Provider Stats', sub: 'Health counters' },
    { id: 'activity', p: 'logs', k: 'activity', icon: 'timeline', label: 'Activity', sub: 'Recent traffic' },
  ]},
  { title: 'Monitoring', k: 'monitoringSection', items: [
    { id: 'logs', p: 'logs', k: 'logs', icon: 'description', label: 'Logs', sub: 'Request ring' },
    { id: 'health', p: 'health', k: 'health', icon: 'health_and_safety', label: 'Health', sub: 'Probes' },
    { id: 'runtime', p: 'runtime', k: 'runtime', icon: 'bolt', label: 'Runtime', sub: 'Process & RSS' },
    { id: 'resilience-connections', p: 'quota', k: 'resilienceConnections', icon: 'shield', label: 'Resilience', sub: 'Cooldowns' },
  ]},
  { title: 'Configuration', k: 'configurationSection', items: [
    { id: 'settings-general', p: 'settings', k: 'settingsGeneral', icon: 'tune', label: 'Settings · General', sub: 'Limits & auth' },
    { id: 'settings-resilience', p: 'quota', k: 'settingsResilience', icon: 'health_and_safety', label: 'Settings · Resilience', sub: 'Cooldown profiles' },
    { id: 'settings-security', p: 'security', k: 'settingsSecurity', icon: 'shield', label: 'Settings · Security', sub: 'Admin password' },
  ]},
  { title: 'Help', items: [
    { id: 'docs', label: 'Docs', k: 'docs', icon: 'menu_book', sub: 'Upstream GitHub', href: 'https://github.com/diegosouzapw/OmniRoute' },
  ]},
];

const EXPANDED_KEY = 'sidebar-expanded-sections';
let expandedSections = null;
function loadExpanded() {
  if (expandedSections) return expandedSections;
  try { expandedSections = new Set(JSON.parse(localStorage.getItem(EXPANDED_KEY) || '[]')); } catch { expandedSections = new Set(); }
  // the original defaults every section to expanded
  if (!localStorage.getItem(EXPANDED_KEY)) NAV.forEach((sec, i) => { if (sec.title) expandedSections.add(i); });
  return expandedSections;
}
function saveExpanded() {
  try { localStorage.setItem(EXPANDED_KEY, JSON.stringify([...expandedSections])); } catch {}
}

function buildSidebar(filter) {
  const nav = $('sidebar-nav');
  const q = (filter ?? '').toLowerCase().trim();
  nav.innerHTML = '';
  const expanded = loadExpanded();
  NAV.forEach((sec, si) => {
    const items = sec.items.filter((it) => !q || (it.label + ' ' + (it.sub || '') + ' ' + (label(it.k, ''))).toLowerCase().includes(q));
    if (!items.length) return;
    const isExp = q ? true : expanded.has(si);
    if (sec.title) {
      const btn = document.createElement('button');
      btn.className = 'grp-toggle';
      btn.innerHTML = `<span class="dotmark"></span><span>${esc(label(sec.k, sec.title))}</span><span class="material-symbols-outlined">${isExp ? 'keyboard_arrow_up' : 'keyboard_arrow_down'}</span>`;
      btn.addEventListener('click', () => {
        if (isExp) expanded.delete(si); else expanded.add(si);
        saveExpanded();
        buildSidebar($('nav-search').value);
      });
      nav.appendChild(btn);
    }
    const wrap = document.createElement('div');
    wrap.className = 'grp-items' + (isExp ? '' : ' collapsed');
    for (const it of items) {
      const a = document.createElement('a');
      if (it.href) { a.target = '_blank'; a.href = it.href; }
      else { a.href = '#' + it.id; a.dataset.page = it.p; }
      const l = label(it.k, it.label);
      const sub = subLabel(it.k, it.sub) || it.sub;
      const icon = it.icon ? `<span class="material-symbols-outlined" style="color:${iconAccent(it.id)}">${esc(it.icon)}</span>` : '';
      a.innerHTML = icon + `<span class="txt"><div>${esc(l)}</div>` + (it.href ? '' : `<span>${esc(sub)}</span>`) + '</span>';
      if (!it.href) {
        if (it.p === CURRENT_PAGE) a.classList.add('active');
        a.addEventListener('click', (e) => {
          e.preventDefault();
          nav.querySelectorAll('a').forEach((x) => x.classList.remove('active'));
          a.classList.add('active');
          setPage(it.p);
        });
      }
      wrap.appendChild(a);
    }
    nav.appendChild(wrap);
  });
}

// ── page registry ──
const PAGES = {};
let CURRENT_PAGE = 'home';
function navItemFor(pageId) {
  for (const sec of NAV) for (const it of sec.items) if (it.p === pageId) return it;
  return null;
}
function setPage(id) {
  const p = PAGES[id];
  if (!p) return;
  CURRENT_PAGE = id;
  const it = navItemFor(id);
  $('page-title').textContent = it ? label(it.k, it.label) : p.title;
  $('page-sub').textContent = it ? (subLabel(it.k, it.sub) || it.sub || '') : '';
  $('page-icon').textContent = (it && it.icon) || 'widgets';
  const iconEl = $('page-icon');
  iconEl.style.color = it ? iconAccent(it.id) : 'var(--color-primary)';
  $('page').innerHTML = p.body();
  if (p.after) p.after();
  window.scrollTo(0, 0);
}

function statusBadge(s) { return `<span class="${s < 400 ? 's-ok' : 's-err'}">${s}</span>`; }
function fmtUptime(s) { return s < 60 ? s + 's' : s < 3600 ? Math.floor(s / 60) + 'm' : Math.floor(s / 3600) + 'h'; }
function fmtKb(kb) { return kb >= 1048576 ? (kb / 1048576).toFixed(1) + ' GB' : Math.round(kb / 1024) + ' MB'; }
function toast(msg, ok = true) {
  const t = $('toast');
  t.textContent = typeof msg === 'string' ? msg : (msg.detail || (msg.ok ? 'saved ✓' : 'error'));
  t.style.background = ok ? 'var(--ok)' : 'var(--bad)';
  t.style.opacity = 1; setTimeout(() => (t.style.opacity = 0), 2400);
}

// ── pages ──
PAGES.home = {
  title: 'Home',
  body: () => {
    const tw = (k, fb) => T('sidebar.' + k) || fb;
    const hw = (k, fb) => T('home.' + k) || fb;
    const cw = (k, fb) => T('common.' + k) || fb;
    const step = (n, icon, color, title, desc) => `
      <div class="qs-step">
        <span class="material-symbols-outlined" style="color:${color}">${icon}</span>
        <div><b>${esc(title)}</b><span>${desc}</span></div>
      </div>`;
    const base = location.origin + '/v1/';
    return `
      <div class="qs-card">
        <div class="qs-head">
          <div>
            <h3>${esc(hw('quickStart', 'Quick start'))}</h3>
            <p>${esc(hw('quickStartDesc', 'Four steps to connect providers, route models and watch the gateway.'))}</p>
          </div>
          <a class="docs-btn save" href="https://github.com/diegosouzapw/OmniRoute" target="_blank" style="text-decoration:none">
            <span class="material-symbols-outlined" style="font-size:16px;vertical-align:-3px">menu_book</span> ${esc(hw('fullDocs', 'Full docs'))}
          </a>
        </div>
        <div class="qs-grid">
          ${step(1, 'vpn_key', 'var(--color-accent-light)', hw('step1Title', '1. Create an API key'), hw('step1Desc', 'Go to <endpoint>Endpoints</endpoint> → registered keys. Issue one key per environment.'))}
          ${step(2, 'dns', '#38d39f', hw('step2Title', '2. Connect a provider'), hw('step2Desc', 'Add an account under Providers. OAuth, API key and free tiers are supported.'))}
          ${step(3, 'terminal', '#f59e0b', hw('step3Title', '3. Configure your client'), hw('step3Desc', 'Point the base URL of your IDE or API client at <code>' + base + '</code>.'))}
          ${step(4, 'monitoring', '#e54d5e', hw('step4Title', '4. Monitor &amp; optimise'), hw('step4Desc', 'Track tokens, cost and errors in the request log and analytics.'))}
        </div>
      </div>
      <div class="panels">
        <div class="panel">
          <h3>${esc(hw('providerTopology', 'Provider topology'))}</h3>
          <div class="panel-sub"><span id="topo-count">…</span></div>
          <div class="legend" style="margin-bottom:12px">
            <span class="l-on"><i></i>${esc(cw('active', 'Active'))}</span>
            <span class="l-recent"><i></i>${esc(hw('topologyLegendRecent', 'Recent'))}</span>
            <span class="l-err"><i></i>${esc(cw('errors', 'Errors'))}</span>
          </div>
          <div class="node-list" id="home-providers"><span class="muted small">loading…</span></div>
        </div>
        <div class="panel">
          <h3>${esc(hw('recentRequests', 'Recent Requests'))}</h3>
          <div class="panel-sub">${esc(cw('time', 'Time'))}</div>
          <table>
            <thead><tr><th>${esc(cw('model', 'Model'))}</th><th>In / Out</th><th>When</th><th></th></tr></thead>
            <tbody id="home-logs"></tbody>
          </table>
        </div>
      </div>`;
  },
  after: async () => {
    const [providers, stats, logs] = await Promise.all([
      api('/v1/providers').catch(() => null),
      api('/v1/stats').catch(() => null),
      api('/v1/logs?limit=8').catch(() => null),
    ]);
    const cw = (k, fb) => T('common.' + k) || fb;
    const hw = (k, fb) => T('home.' + k) || fb;
    if (stats) {
      const cards = [
        [fmtUptime(stats.uptime_s ?? 0), cw('uptime', 'uptime')],
        [stats.requests ?? 0, cw('requests', 'requests')],
        [stats.failures ?? 0, cw('errors', 'errors')],
        [fmtKb(stats.memory_kb ?? 0), 'RSS'],
      ];
      const el = document.createElement('div');
      el.className = 'cards';
      el.innerHTML = cards.map(([n, l]) => `<div class="card"><div class="n">${n}</div><div class="l">${esc(l)}</div></div>`).join('');
      $('page').insertBefore(el, $('page').querySelector('.panels'));
    }
    if (providers) {
      const on = providers.providers.filter((p) => p.cooldownMs === 0 && p.hasKey);
      const err = providers.providers.filter((p) => p.cooldownMs > 0 || !p.hasKey);
      $('topo-count').textContent = `${on.length} ${cw('active', 'active')} · ${err.length} ${cw('errors', 'errors')}`;
      const list = $('home-providers');
      list.innerHTML = providers.providers.length
        ? providers.providers.map((p) => {
            const ok = p.cooldownMs === 0 && p.hasKey;
            return `<div class="node">
              <span class="material-symbols-outlined" style="font-size:16px;color:${ok ? '#22c55e' : (p.cooldownMs > 0 ? '#ef4444' : '#f59e0b')}">${ok ? 'check_circle' : 'error'}</span>
              <span class="nm">${esc(p.id)}</span>
              <span class="badge">${esc(p.format)}</span>
              <span class="meta">${p.inFlight} in-flight${p.cooldownMs > 0 ? ' · cooldown ' + p.cooldownMs + 'ms' : ''}${p.hasKey ? '' : ' · no key'}</span>
            </div>`;
          }).join('')
        : `<div class="na-note">${esc(cw('providerTopologyEmpty', 'No providers connected yet'))}</div>`;
    }
    if (logs) {
      const rows = logs.logs || [];
      $('home-logs').innerHTML = rows.length
        ? rows.map((l) => `<tr>
            <td>${esc(l.model)}</td>
            <td>${l.prompt_tokens ?? 0} | ${l.completion_tokens ?? 0}</td>
            <td>${relTime(l.ts_ms)}</td>
            <td><button class="row-menu" title="details">…</button></td>
          </tr>`).join('')
        : `<tr><td colspan="4" class="muted small">${esc(cw('noData', 'no data'))}</td></tr>`;
    }
  },
};

function relTime(ts) {
  const s = Math.max(0, Math.round((Date.now() - ts) / 1000));
  if (s < 60) return s + 's';
  if (s < 3600) return Math.round(s / 60) + 'm';
  return Math.round(s / 3600) + 'h';
}

const ENDPOINT_ROWS = [
  ['POST', '/v1/chat/completions', 'chat (openai wire)'],
  ['POST', '/v1/completions', 'legacy completions'],
  ['POST', '/v1/responses', 'openai-responses'],
  ['POST', '/v1/messages, /v1/messages/count_tokens', 'anthropic-native'],
  ['GET', '/v1, /v1/models', 'combined model catalog'],
  ['POST', '/v1/embeddings, /v1/rerank, /v1/moderations', 'single-provider passthrough'],
  ['POST', '/v1/images/{generations,edits,upscale}', 'passthrough'],
  ['POST', '/v1/audio/{transcriptions,translations,speech}', 'raw passthrough (multipart preserved)'],
  ['POST', '/v1/speech-to-text, /v1/text-to-speech, /v1/videos, /v1/ocr, /v1/files', 'passthrough'],
  ['POST', '/v1/batches · GET /v1/batches{,/{id}}', 'provider via x-omniroute-provider header'],
  ['GET', '/v1/stats · /v1/logs · /v1/settings · /v1/compression', 'dashboard APIs'],
  ['POST', '/v1/auth/{login,logout,change-password}', 'account auth'],
  ['GET', '/v1/api-keys, /v1/provider-connections{,/{id}/test}', 'management'],
  ['GET', '/healthz /readyz /livez /api/health', 'probes'],
];
PAGES.endpoints = {
  title: 'Endpoints',
  body: () => `
    <h1>Endpoints</h1>
    <p class="muted small">AI surfaces served by this omniroute-rust build</p>
    <table><thead><tr><th>method</th><th>path</th><th>notes</th></tr></thead><tbody>
    ${ENDPOINT_ROWS.map(([m, p, n]) => `<tr><td>${esc(m)}</td><td>${esc(p)}</td><td class="muted">${esc(n)}</td></tr>`).join('')}
    </tbody></table>
    <h2>Not in omniroute-rust build</h2>
    <p class="na-note">/v1/ws (routing WS) · webhooks editor · embedded browser-services executors (antigravity / grok-web / claude-web / cursor web flows) · a2a · mcp stdio engine · gamification · media-providers pipelines · batch orchestration UI.</p>`,
};

PAGES.apikeys = {
  title: 'API Manager',
  body: () => `
    <h1>API Manager</h1>
    <p class="muted small">Client API keys for agents (Claude Code, Cline, ...). Inference requires one of these (or the master key) once any exists.</p>
    <div style="margin-bottom:12px"><button class="save" id="key-new">+ Create key</button></div>
    <table><thead><tr><th>name</th><th>key</th><th>role</th><th>enabled</th><th>requests</th><th>actions</th></tr></thead><tbody id="key-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/api-keys');
    $('key-rows').innerHTML = v.api_keys.length
      ? v.api_keys.map((k) =>
          `<tr><td>${esc(k.name)}</td><td>${esc(k.key)}</td><td>${esc(k.role)}</td><td>${k.enabled}</td><td>${k.total_requests}</td>` +
          `<td><button class="mini" data-kid="${k.id}" data-on="${k.enabled}">${k.enabled ? 'revoke' : 'restore'}</button></td></tr>`).join('')
      : '<tr><td colspan="6">no api keys exist — inference open until one is created</td></tr>';
    $('key-rows').querySelectorAll('button').forEach((b) => b.addEventListener('click', async () => {
      await api('/v1/api-keys/' + b.dataset.kid, { method: 'PATCH', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ enabled: !(b.dataset.on === 'true') }) });
      PAGES.apikeys.after();
    }));
    $('key-new').onclick = async () => {
      const name = prompt('key name:') || 'default';
      const role = prompt('role default|admin:', 'default') || 'default';
      if (name.trim().length > 200) { toast('name too long (max 200)', false); return; }
      const v = await api('/v1/api-keys', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ name, role }) });
      toast('new key (shown once): ' + v.api_key.key);
      PAGES.apikeys.after();
    };
  },
};

PAGES.providers = {
  title: 'Providers',
  body: () => `
    <h1>Providers</h1>
    <h2>Managed connections</h2>
    <div id="provider-managed"><span class="muted">loading…</span></div>
    <h2>Add / override</h2>
    <div class="combo">
      <label>provider (anthropic · openai · gemini · openai-compatible-myrelay …)</label>
      <input type="text" id="p-provider" style="width:420px">
      <label>display name</label><input type="text" id="p-name" style="width:420px">
      <label>api key</label><input type="text" id="p-key" style="width:420px">
      <label>base url (needed for *-compatible families)</label><input type="text" id="p-base" style="width:420px" placeholder="https://...">
      <label>models (comma separated)</label><input type="text" id="p-models" style="width:420px" placeholder="gpt-4o, claude-3">
      <div><button class="save" id="p-add">Add / override</button></div>
    </div>
    <h2>Model catalog</h2>
    <input type="text" id="model-search" placeholder="filter models…" autocomplete="off" style="width:320px;margin-bottom:10px">
    <table><thead><tr><th>id</th><th>provider</th><th>context</th></tr></thead><tbody id="provider-rows"></tbody></table>`,
  after: async () => {
    let models = [];
    try { models = (await api('/v1/models')).data || []; } catch { models = []; }
    const managed = await api('/v1/provider-connections').catch(() => null);
    $('provider-managed').innerHTML = managed && managed.connections.length
      ? managed.connections.map((c) => `
        <div class="combo"><b>${esc(c.name)}</b> <span class="badge">${esc(c.provider)}</span>${c.enabled ? '' : '<span class="badge">disabled</span>'}
          <div class="chain">key ${esc(c.api_key || 'none')} · base ${esc(c.baseUrl || 'registry default')} · ${esc((c.models || []).join(', ') || 'no models')}</div>
          <div class="row-actions"><button class="mini" data-act="test" data-id="${c.id}">test</button><button class="mini" data-act="del" data-id="${c.id}">remove</button> <span class="p-result"></span></div>
        </div>`).join('')
      : '<div class="na-note">no managed connections — use the form below (credentials persist to provider-connections.json)</div>';
    const draw = () => {
      const q = ($('model-search')?.value || '').toLowerCase();
      $('provider-rows').innerHTML = models
        .filter((m) => !q || m.id.toLowerCase().includes(q))
        .slice(0, 300)
        .map((m) => `<tr><td>${esc(m.id)}</td><td>${esc(m.provider)}</td><td>${m.contextLength}</td></tr>`).join('');
    };
    $('model-search').addEventListener('input', draw);
    draw();
    $('p-add').addEventListener('click', async () => {
      try {
        await api('/v1/provider-connections', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({
          provider: $('p-provider').value, name: $('p-name').value, api_key: $('p-key').value || null,
          base_url: $('p-base').value || null, model_list: ($('p-models').value || '').split(',').map((s) => s.trim()).filter(Boolean),
        }) });
        toast('provider connection saved');
        PAGES.providers.after();
      } catch { toast('save failed', false); }
    });
    $('provider-managed').querySelectorAll('button').forEach((b) => b.addEventListener('click', async () => {
      if (b.dataset.act === 'del') {
        await api('/v1/provider-connections/' + b.dataset.id, { method: 'DELETE' });
        PAGES.providers.after();
        return;
      }
      const v = await api('/v1/provider-connections/' + b.dataset.id + '/test', { method: 'POST' });
      const span = b.parentElement.querySelector('.p-result');
      span.innerHTML = v.ok ? `<span class="s-ok">ok · ${v.latency_ms}ms</span>` : `<span class="s-err">fail: ${esc((v.detail || '').slice(0, 90))}</span>`;
    }));
  },
};

PAGES.combos = {
  title: 'Combos',
  body: () => `
    <h1>Combos</h1>
    <p class="muted small">Routing chains from omniroute.toml [[combos]] — provider/model target list with strategy</p>
    <div id="combo-list"><span class="muted">loading…</span></div>`,
  after: async () => {
    const v = await api('/v1/combos');
    $('combo-list').innerHTML = v.combos.length
      ? v.combos.map((c) =>
          `<div class="combo"><b>${esc(c.name)}</b> <span class="badge">${esc(c.strategy)}</span><div class="chain">${c.providers.map(esc).join(' → ')}</div></div>`).join('')
      : '<div class="na-note">no combos configured — add [[combos]] to omniroute.toml and restart</div>';
  },
};

PAGES.quota = {
  title: 'Provider Quota & Resilience',
  body: () => `
    <h1>Provider Quota & Resilience</h1>
    <table><thead><tr><th>provider</th><th>rpm budget</th><th>rpm window</th><th>concurrent</th><th>cooldown</th></tr></thead><tbody id="quota-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/quotas');
    $('quota-rows').innerHTML = v.quotas.map((q) =>
      `<tr><td>${esc(q.provider)}</td><td>${q.rpmBudget}</td><td>${q.rpmWindowHits}</td><td>${q.concurrent}</td><td>${q.cooldownMs > 0 ? `<span class="s-err">${q.cooldownMs}ms</span>` : '0'}</td></tr>`).join('');
  },
};

PAGES.usage = {
  title: 'Usage',
  body: () => `
    <h1>Usage</h1>
    <div class="cards" id="usage-cards"></div>
    <table><thead><tr><th>provider</th><th>requests</th></tr></thead><tbody id="usage-rows"></tbody></table>`,
  after: async () => {
    const logs = (await api('/v1/logs?limit=500').catch(() => ({ logs: [] }))).logs || [];
    const byProvider = {};
    let okCount = 0, saved = 0;
    for (const l of logs) {
      const p = l.provider || '(unrouted)';
      byProvider[p] = (byProvider[p] || 0) + 1;
      if (l.status < 400) okCount++;
      saved += l.tokens_saved || 0;
    }
    $('usage-cards').innerHTML = [
      [logs.length, 'requests sampled'], [okCount, 'succeeded'],
      [logs.length - okCount, 'failures', (logs.length - okCount) > 0 ? 'n err' : 'n'],
      [saved, 'tokens saved (compression)'],
    ].map(([n, l, cls]) => `<div class="card"><div class="${cls || 'n'}">${n}</div><div class="l">${l}</div></div>`).join('');
    $('usage-rows').innerHTML = Object.entries(byProvider).map(([p, n]) =>
      `<tr><td>${esc(p)}</td><td>${n}</td></tr>`).join('');
  },
};

PAGES.logs = {
  title: 'Logs',
  body: () => `
    <h1>Request log</h1>
    <p class="muted small">in-memory ring (last 500); proxy/console/timeline differentiation ships with the Next.js original</p>
    <table><thead><tr><th>time</th><th>model</th><th>provider</th><th>status</th><th>ms</th><th>tokens saved</th></tr></thead><tbody id="log-rows"></tbody></table>`,
  after: async () => {
    const logs = await api('/v1/logs?limit=200');
    $('log-rows').innerHTML = (logs.logs || []).map((l) =>
      `<tr><td>${new Date(l.ts_ms).toLocaleTimeString()}</td><td>${esc(l.model)}</td><td>${esc(l.provider || '-')}</td><td>${statusBadge(l.status)}</td><td>${l.latency_ms}</td><td>${l.tokens_saved || 0}</td></tr>`).join('');
  },
};

PAGES.health = {
  title: 'System Health',
  body: () => `
    <h1>System health</h1>
    <table><thead><tr><th>probe</th><th>status</th></tr></thead><tbody id="health-rows"></tbody></table>`,
  after: async () => {
    const rows = [];
    for (const probe of ['healthz', 'readyz', 'livez']) {
      try { const r = await fetch('/' + probe); rows.push([probe, r.ok ? '<span class="s-ok">ok</span>' : '<span class="s-err">HTTP ' + r.status + '</span>']); }
      catch { rows.push([probe, '<span class="s-err">unreachable</span>']); }
    }
    const c = await api('/v1/compression');
    rows.push(['compression engine', `<span class="badge">${esc(c.default_mode)}</span> enabled=${esc(c.enabled)}`]);
    rows.push(['auth mode', 'session + managed keys']);
    $('health-rows').innerHTML = rows.map(([p, s]) => `<tr><td>${esc(p)}</td><td>${s}</td></tr>`).join('');
  },
};

PAGES.runtime = {
  title: 'Runtime',
  body: () => `
    <h1>Runtime</h1>
    <table><thead><tr><th>metric</th><th>value</th></tr></thead><tbody id="rt-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/stats');
    $('rt-rows').innerHTML = [
      ['pid', v.pid], ['uptime', fmtUptime(v.uptime_s ?? 0)], ['requests', v.requests],
      ['failures', v.failures], ['gateway RSS', fmtKb(v.memory_kb ?? 0)],
      ['providers configured', v.providers_with_keys], ['models', v.models],
    ].map(([k, val]) => `<tr><td>${esc(k)}</td><td>${esc(val)}</td></tr>`).join('');
  },
};

PAGES.compression = {
  title: 'Compression',
  body: () => `
    <h1>Compression</h1>
    <p class="muted small">engines: lite (RTK minimal) · standard (Caveman rules) · aggressive (summarizer) · ultra (score pruning) · rtk (output filters)</p>
    <div id="compression-env"><span class="muted">loading…</span></div>
    <h2>Change at runtime</h2>
    <div id="compression-form"><span class="muted">loading…</span></div>`,
  after: async () => {
    const c = await api('/v1/compression');
    const names = { lite: 'RTK lite', standard: 'Caveman rules', aggressive: 'Summarizer', ultra: 'Score pruning', rtk: 'RTK filters', off: 'disabled' };
    $('compression-env').innerHTML = `
      <div class="combo"><b>${esc(names[c.default_mode] || c.default_mode)}</b> <span class="badge">${esc(c.default_mode)}</span> &nbsp; enabled: ${esc(c.enabled)}
      <div class="chain">per-request override: <b>x-omniroute-compression</b> header (off|default|lite|standard|aggressive|ultra|rtk)</div>
      </div>`;
    $('compression-form').innerHTML = `
      <div class="combo">
        <label>enabled</label><select id="c-enabled"><option value="true" ${c.enabled ? 'selected' : ''}>true</option><option value="false" ${!c.enabled ? 'selected' : ''}>false</option></select><br>
        <label>default_mode</label><select id="c-mode">${c.modes.map((m) => `<option ${m === c.default_mode ? 'selected' : ''}>${m}</option>`).join('')}</select><br>
        <label>auto_trigger_tokens</label><input type="number" id="c-auto" value="${c.auto_trigger_tokens}"><br>
        <label>caveman_intensity</label><select id="c-intensity">${['lite', 'full', 'ultra'].map((m) => `<option ${m === c.caveman_intensity ? 'selected' : ''}>${m}</option>`).join('')}</select><br>
        <label>preserve_system_prompt</label><select id="c-sys"><option value="true" ${c.preserve_system_prompt ? 'selected' : ''}>true</option><option value="false" ${c.preserve_system_prompt ? 'selected' : ''}>false</option></select><br>
        <label>min_message_length</label><input type="number" id="c-minlen" value="${c.min_message_length}"><br>
        <label>ultra_compression_rate</label><input type="number" step="0.05" id="c-rate" value="${c.ultra_compression_rate}"><br>
        <label>rtk_max_lines</label><input type="number" id="c-rtk" value="${c.rtk_max_lines}"><br>
        <div><button class="save" id="c-save">Save</button></div>
      </div>`;
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
      try {
        await api('/v1/compression', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
        toast('compression saved');
      } catch { toast('save failed', false); }
    });
  },
};

PAGES.settings = {
  title: 'Settings · General',
  body: () => `
    <h1>Settings · General</h1>
    <h2>Rate limits (server runtime)</h2>
    <table><thead><tr><th>metric</th><th>value</th></tr></thead><tbody id="set-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/settings');
    $('set-rows').innerHTML = [
      ['requests per minute', v.rate_rpm], ['min interval ms', v.rate_min_interval_ms],
      ['max concurrent per provider', v.rate_concurrent_requests], ['max queue wait ms', v.rate_max_wait_ms],
      ['compression default mode', v.compression_default_mode], ['api auth mode', v.api_auth],
    ].map(([k, val]) => `<tr><td>${esc(k)}</td><td>${esc(val)}</td></tr>`).join('');
  },
};

PAGES['settings-resilience'] = PAGES.quota;

PAGES.security = {
  title: 'Settings · Security',
  body: () => `
    <h1>Settings · Security</h1>
    <h2>Admin password</h2>
    <div class="combo">
      <label>current password</label><input type="password" id="sec-cur" style="width:280px"><br>
      <label>new password (min 8)</label><input type="password" id="sec-new" style="width:280px"><br>
      <div><button class="save" id="sec-save">Change password</button></div>
      <div class="muted small" style="margin-top:8px">Reminder: <b>OMNIROUTE_ADMIN_PASSWORD</b> env wins on restart.</div>
    </div>`,
  after: async () => {
    $('sec-save').addEventListener('click', async () => {
      try {
        await api('/v1/auth/change-password', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ current_password: $('sec-cur').value, new_password: $('sec-new').value }) });
        toast('password changed');
      } catch { toast('change failed', false); }
    });
  },
};

// ── health polling ──
async function pollHealth() {
  try {
    const r = await fetch('/healthz');
    $('health-dot').className = 'dot' + (r.ok ? ' ok' : '');
    $('health-dot').title = r.ok ? 'healthy' : 'HTTP ' + r.status;
  } catch {
    $('health-dot').className = 'dot';
    $('health-dot').title = 'unreachable';
  }
}

function showLogin(need) {
  $('login-screen').style.display = need ? 'flex' : 'none';
  $('app').style.display = need ? 'none' : 'flex';
  $('logout').classList.toggle('hidden', need);
}
function showDefaultBanner(need) { $('default-pw-banner').style.display = need ? 'flex' : 'none'; }

// ── auth boot (single writer of the login overlay) ──
async function bootAuth() {
  try {
    const r = await fetch('/v1/auth/me');
    const me = await r.json();
    showLogin(me.login_required === true);
    showDefaultBanner(me.using_default_password === true);
    return me.authenticated === true;
  } catch { showLogin(true); return false; }
}

(async () => {
  try {
    const hw = await fetch('/dashboard/languages.json');
    PACK = await (await fetch('/dashboard/locales/en.json')).json();
    LANGSGLOBAL = await hw.json();
  } catch {}
  // login handlers
  $('login-btn').addEventListener('click', async () => {
    try {
      const r = await fetch('/v1/auth/login', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ password: $('login-password').value }) });
      if (r.ok) {
        const v = await r.json();
        localStorage.setItem('omniroute_session', v.token);
        $('login-error').textContent = '';
        showLogin(false);
        setPage('home');
      } else $('login-error').textContent = r.status === 401 ? 'invalid password' : 'HTTP ' + r.status;
    } catch (e) { $('login-error').textContent = String(e); }
  });
  $('login-password').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('login-btn').click(); });
  $('logout').classList.add('hidden');
  $('logout').addEventListener('click', async () => {
    try { await fetch('/v1/auth/logout', { method: 'POST' }); } catch {}
    localStorage.removeItem('omniroute_session');
    showLogin(true);
  });
  $('sidebar-toggle').addEventListener('click', () => { $('sidebar').classList.toggle('hidden'); });
  $('pw-change').addEventListener('click', async () => {
    const np = $('new-pw').value;
    if (np.length < 8) { $('pw-msg').innerHTML = '<span class="s-err">min 8 chars</span>'; return; }
    const tok = localStorage.getItem('omniroute_session') || '';
    const r = await fetch('/v1/auth/change-password', {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: 'Bearer ' + tok },
      body: JSON.stringify({ current_password: 'CHANGEME', new_password: np }),
    });
    if (r.ok) { showDefaultBanner(false); toast('admin password changed'); }
    else { $('pw-msg').innerHTML = '<span class="s-err">change failed (HTTP ' + r.status + ')</span>'; }
  });
  // language selector (flag + native name picks, LanguageSelector parity)
  const RTL = ['ar', 'fa', 'he', 'ur'];
  const setLang = async (code) => {
    await loadPack(code);
    localStorage.setItem('omniroute_locale', code);
    document.cookie = 'omniroute_locale=' + code + '; Path=/dashboard; Max-Age=31536000; SameSite=Lax';
    document.documentElement.lang = code;
    document.documentElement.dir = RTL.includes(code) ? 'rtl' : 'ltr';
    const cur = langs.find((l) => l.code === code) || {};
    $('lang-flag').textContent = cur.flag || '🌐';
    $('lang-label').textContent = (cur.code || code).toUpperCase();
    try {
      $('svc-restart-label').textContent = T('sidebar.restart') || 'Restart';
      $('svc-stop-label').textContent = T('sidebar.shutdown') || 'Stop';
      $('quick-nav-label').textContent = T('common.quickNavigation') || 'Quick navigation';
      $('nav-search').placeholder = T('common.search') || 'Search';
    } catch {}
    buildSidebar($('nav-search') ? $('nav-search').value : '');
    setPage(CURRENT_PAGE || 'home');
  };
  const langs = await (await fetch('/dashboard/languages.json')).json();
  const current_page = 'home';

  $('lang-selector').style.display = '';
  $('lang-selector').addEventListener('click', async () => {
    // open picker modal (flag/native/english rows)
    $('modal-card').innerHTML = '<h2>Language</h2>';
    const box = document.createElement('div');
    box.style.maxHeight = '400px'; box.style.overflowY = 'auto';
    for (const l of langs) {
      const item = document.createElement('a');
      item.dataset.l = l.code;
      item.style.cssText = 'display:flex;gap:10px;padding:6px 12px;cursor:pointer';
      item.innerHTML = `<span>${esc(l.flag || '')}</span><b>${esc(l.native || l.name || l.code)}</b><span class="muted small">${esc(l.english || '')}</span>`;
      item.addEventListener('click', () => {
        $('modal').style.display = 'none';
        setLang(l.code);
      });
      box.appendChild(item);
    }
    $('modal-card').appendChild(box);
    $('modal').style.display = 'flex';
  });
  // ── chrome wiring: sidebar search, theme, quick nav, service actions ──
  $('nav-search').addEventListener('input', () => buildSidebar($('nav-search').value));
  const applyTheme = (t) => {
    document.documentElement.dataset.theme = t;
    $('theme-icon').textContent = t === 'light' ? 'dark_mode' : 'light_mode';
    localStorage.setItem('omniroute_theme', t);
  };
  applyTheme(localStorage.getItem('omniroute_theme') || 'dark');
  $('theme-toggle').addEventListener('click', () => {
    applyTheme(document.documentElement.dataset.theme === 'light' ? 'dark' : 'light');
  });
  const openPalette = () => {
    $('modal-card').innerHTML = `<h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main)">${esc(T('common.quickNavigation') || 'Quick navigation')}</h2>`;
    const inp = document.createElement('input');
    inp.type = 'text';
    inp.placeholder = T('common.search') || 'Search';
    inp.style.width = '100%';
    const box = document.createElement('div');
    box.style.cssText = 'max-height:60vh;overflow:auto;margin-top:10px';
    const items = [];
    for (const sec of NAV) for (const it of sec.items) if (!it.grp && !it.href) items.push({ it, sec });
    const render = (q) => {
      box.innerHTML = '';
      for (const { it, sec } of items) {
        const l = label(it.k, it.label);
        if (q && !(l + ' ' + (it.sub || '')).toLowerCase().includes(q)) continue;
        const row = document.createElement('a');
        row.style.cssText = 'display:flex;gap:10px;align-items:center;padding:7px 10px;border-radius:8px;cursor:pointer';
        row.innerHTML = `<span class="material-symbols-outlined" style="font-size:17px;color:${iconAccent(it.id)}">${esc(it.icon || 'widgets')}</span>` +
          `<b style="font-weight:500">${esc(l)}</b><span class="muted small" style="margin-left:auto">${esc(sec.title ? label(sec.k, sec.title) : '')}</span>`;
        row.addEventListener('click', () => { $('modal').style.display = 'none'; setPage(it.p); });
        box.appendChild(row);
      }
    };
    inp.addEventListener('input', () => render(inp.value.toLowerCase().trim()));
    $('modal-card').appendChild(inp);
    $('modal-card').appendChild(box);
    render('');
    $('modal').style.display = 'flex';
    inp.focus();
  };
  $('quick-nav').addEventListener('click', openPalette);
  document.addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); openPalette(); }
    if (e.key === 'Escape') $('modal').style.display = 'none';
  });
  const serviceAction = async (action) => {
    const which = action === 'restart' ? (T('sidebar.restart') || 'Restart service') : (T('sidebar.shutdown') || 'Stop service');
    if (!confirm(which + '?')) return;
    try {
      const r = await fetch('/v1/admin/service/' + action, {
        method: 'POST',
        headers: { authorization: 'Bearer ' + (localStorage.getItem('omniroute_session') || '') },
      });
      toast(r.ok ? which + ' …' : 'HTTP ' + r.status, r.ok);
    } catch (e) { toast(String(e), false); }
  };
  $('svc-restart').addEventListener('click', () => serviceAction('restart'));
  $('svc-stop').addEventListener('click', () => serviceAction('stop'));
  $('power-btn').addEventListener('click', () => serviceAction('restart'));

  // auth boot decides login screen
  const savedLocale = localStorage.getItem('omniroute_locale') || 'en';
  await setLang(savedLocale);
  const first = $('sidebar-nav').querySelector('a[data-page]');
  if (first) first.classList.add('active');
  const authed = await bootAuth();
  pollHealth();
  setInterval(pollHealth, 5000);
  if (authed) setPage('home');
  try { const h = await api('/api/health'); $('sidebar-ver').textContent = 'v' + (h.version || '?'); } catch {}
  if ('serviceWorker' in navigator) navigator.serviceWorker.register('/dashboard/sw.js').catch(() => {});
})();
