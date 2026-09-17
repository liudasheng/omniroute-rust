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
    const tok = localStorage.getItem('omniroute_session') || '';
    const me = await fetch('/v1/auth/me', tok ? { headers: { authorization: 'Bearer ' + tok } } : {}).then((x) => x.json());
    if (!me.authenticated) {
      if (tok) localStorage.removeItem('omniroute_session');
      showLogin(true); throw new Error(path + ': 401');
    }
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
    { id: 'context-headroom', p: 'compression', k: 'contextHeadroom', icon: 'table_rows', label: 'Headroom', sub: 'Tabular compaction' },
    { id: 'context-rtk', p: 'compression', k: 'contextRtk', icon: 'filter_alt', label: 'RTK', sub: 'Output filters' },
    { id: 'context-combos', p: 'compression', k: 'contextCombos', icon: 'layers', label: 'Engine stacks', sub: 'Compression chains' },
    { id: 'context-ultra', p: 'compression', k: 'contextUltra', icon: 'bolt', label: 'Ultra', sub: 'Heuristic pruning' },
    { id: 'context-aggressive', p: 'compression', k: 'contextAggressive', icon: 'speed', label: 'Aggressive', sub: 'Summary + aging' },
    { id: 'context-lite', p: 'compression', k: 'contextLite', icon: 'compress', label: 'Lite', sub: 'Whitespace cleanup' },
    { title: 'Tools', k: 'toolsGroup', grp: true },
    { id: 'playground', p: 'playground', k: 'playground', icon: 'science', label: 'Playground', sub: 'Send a chat request' },
    { id: 'combos-live', p: 'combostudio', k: 'combosLive', icon: 'hub', label: 'Combos Studio', sub: 'Live routing view' },
    { id: 'embedded-services', p: 'embeddedservices', k: 'embeddedServices', icon: 'deployed_code', label: 'Embedded services', sub: 'Local executors' },
    { id: 'translator', p: 'translator', k: 'translator', icon: 'translate', label: 'Translator', sub: 'Format conversion' },
    { id: 'batch', p: 'batch', k: 'batch', icon: 'table_view', label: 'Batch', sub: 'Batch API status' },
    { id: 'traffic-inspector', p: 'logs', k: 'trafficInspector', icon: 'visibility', label: 'Traffic inspector', sub: 'Request details' },
  ]},
  { title: 'Analytics', k: 'analyticsSection', items: [
    { id: 'usage', p: 'usage', k: 'usage', icon: 'analytics', label: 'Usage', sub: 'Request analytics' },
    { id: 'combo-health', p: 'combohealth', k: 'analyticsComboHealth', icon: 'monitor_heart', label: 'Combo Health', sub: 'Success & latency' },
    { id: 'utilization', p: 'utilization', k: 'analyticsUtilization', icon: 'speed', label: 'Utilization', sub: 'Rate-limit usage' },
    { id: 'cache', p: 'cachehealth', k: 'cache', icon: 'database', label: 'Cache Health', sub: 'Dedup effectiveness' },
    { id: 'route-tracing', p: 'routingtrace', k: 'radar', icon: 'route', label: 'Route tracing', sub: 'Routing decisions' },
    { id: 'analytics-compression', p: 'compressionstats', k: 'analyticsCompression', icon: 'compress', label: 'Compression', sub: 'Tokens saved' },
    { id: 'provider-stats', p: 'providerstats', k: 'providerStats', icon: 'dns', label: 'Provider Stats', sub: 'Health counters' },
    { id: 'free-tiers', p: 'freetiers', k: 'freeTiers', icon: 'savings', label: 'Free tiers', sub: 'Free-tier catalog' },
    { id: 'activity', p: 'logs', k: 'activity', icon: 'timeline', label: 'Activity', sub: 'Recent traffic' },
  ]},
  { title: 'Monitoring', k: 'monitoringSection', items: [
    { id: 'logs', p: 'logs', k: 'logs', icon: 'description', label: 'Logs', sub: 'Request ring' },
    { id: 'log-export', p: 'logexport', k: 'logExport', icon: 'download', label: 'Log export', sub: 'CSV / JSON' },
    { id: 'audit-log', p: 'audit', k: 'auditLog', icon: 'history', label: 'Audit log', sub: 'Management actions' },
    { id: 'health', p: 'health', k: 'health', icon: 'health_and_safety', label: 'Health', sub: 'Probes' },
    { id: 'runtime', p: 'runtime', k: 'runtime', icon: 'bolt', label: 'Runtime', sub: 'Process & RSS' },
    { id: 'resilience-connections', p: 'resilience', k: 'resilienceConnections', icon: 'shield', label: 'Resilience', sub: 'Cooldowns' },
  ]},
  { title: 'Configuration', k: 'configurationSection', items: [
    { id: 'settings-general', p: 'settings', k: 'settingsGeneral', icon: 'tune', label: 'Settings · General', sub: 'Limits & auth' },
    { id: 'settings-appearance', p: 'appearance', k: 'settingsAppearance', icon: 'palette', label: 'Settings · Appearance', sub: 'Theme & language' },
    { id: 'settings-sidebar', p: 'sidebarsettings', k: 'settingsSidebar', icon: 'view_sidebar', label: 'Settings · Sidebar', sub: 'Visible sections' },
    { id: 'quota-share', p: 'quotashare', k: 'costsQuotaShare', icon: 'share', label: 'Quota share', sub: 'Budget across keys' },
    { id: 'settings-resilience', p: 'resilience', k: 'settingsResilience', icon: 'health_and_safety', label: 'Settings · Resilience', sub: 'Cooldown profiles' },
    { id: 'settings-security', p: 'security', k: 'settingsSecurity', icon: 'shield', label: 'Settings · Security', sub: 'Password & auth' },
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
  let hiddenIds = [];
  try { hiddenIds = JSON.parse(localStorage.getItem('omniroute_hidden_nav') || '[]'); } catch {}
  NAV.forEach((sec, si) => {
    const items = sec.items
      .filter((it) => !hiddenIds.includes(it.id))
      .filter((it) => !q || (it.label + ' ' + (it.sub || '') + ' ' + (label(it.k, ''))).toLowerCase().includes(q));
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
  const sectionOf = (() => {
    for (const sec of NAV) for (const it of sec.items) if (it.p === id) return sec;
    return null;
  })();
  const crumb = `<div class="breadcrumb"><a href="#home">${esc(T('sidebar.home') || 'Home')}</a>
    <span class="material-symbols-outlined" style="font-size:14px">chevron_right</span>
    <span>${esc((sectionOf && sectionOf.title) ? label(sectionOf.k, sectionOf.title) : '')}</span>
    <span class="material-symbols-outlined" style="font-size:14px">chevron_right</span>
    <b>${esc(it ? label(it.k, it.label) : p.title)}</b></div>`;
  $('page').innerHTML = crumb + p.body();
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
  body: () => {
    const pw = (k, fb) => T('sidebar.' + k) || fb;
    const cw = (k, fb) => T('common.' + k) || fb;
    const ew = (k, fb) => T('endpoints.' + k) || fb;
    return `
    <h1>${esc(ew('title', 'API endpoints'))}</h1>
    <p class="muted small">${esc(ew('desc', 'Use the OpenAI-compatible endpoint with most SDKs and tools.'))}</p>
    <div class="flow" style="margin:10px 0 4px">
      <code id="ep-base">—</code>
      <a href="#" id="ep-test">${esc(ew('testEndpoint', 'Test endpoint'))} →</a>
    </div>
    <div class="muted small" style="margin-bottom:6px">${esc(ew('advancedProtocols', 'Advanced protocols'))}</div>
    <div class="tabbar" id="ep-protocols">
      <button class="active" data-p="openai"><span class="material-symbols-outlined">hub</span>${esc(ew('openaiCompatible', 'OpenAI compatible'))}</button>
      <button data-p="mcp"><span class="material-symbols-outlined">extension</span>MCP</button>
      <button data-p="a2a"><span class="material-symbols-outlined">share</span>A2A</button>
      <button data-p="context"><span class="material-symbols-outlined">database</span>${esc(ew('contextSources', 'Context sources'))}</button>
    </div>
    <div id="ep-proto-note" class="na-note" style="display:none"></div>

    <div class="section-card">
      <h3 style="margin:0 0 10px">${esc(ew('apiEndpoints', 'API endpoints'))}</h3>
      <div class="active-endpoints">
        <div class="active-title">${esc(ew('activeEndpoints', 'Active endpoints'))}</div>
        <div class="active-row"><span class="dot ok"></span><b>${esc(ew('public', 'Public'))}</b>
          <code id="ep-public">—</code>
          <button class="icon-btn" data-copy-el="ep-public"><span class="material-symbols-outlined">content_copy</span></button></div>
        <div class="active-row"><span class="dot ok"></span><b>${esc(ew('local', 'Local'))}</b>
          <code id="ep-local">—</code>
          <button class="icon-btn" data-copy-el="ep-local"><span class="material-symbols-outlined">content_copy</span></button></div>
      </div>
      <div class="endpoint-row">
        <span class="material-symbols-outlined">dns</span>
        <b>${esc(ew('localServer', 'Local server'))}</b>
        <span class="muted small" id="ep-sid">—</span>
        <code id="ep-surl">—</code>
        <button class="icon-btn" data-copy-el="ep-surl"><span class="material-symbols-outlined">content_copy</span></button>
        <span class="status-pill healthy" style="margin-left:auto"><i></i>${esc(ew('running', 'running'))}</span>
        <button class="mini" data-copy-el="ep-surl">${esc(cw('copy', 'Copy'))}</button>
      </div>
      <div class="endpoint-row">
        <span class="material-symbols-outlined">link</span><b>${esc(ew('tunnels', 'Tunnels'))}</b>
        <span class="tag" id="ep-tunnel-count" style="margin-left:auto">—</span>
      </div>
      <div id="ep-tunnels"></div>
      <div class="endpoint-row">
        <span class="material-symbols-outlined">psychology</span>
        <div><b>${esc(ew('customPrompt', 'Custom system prompt'))}</b>
          <div class="muted small">${esc(ew('customPromptDesc', 'Inject a custom system prompt into every model request.'))}</div></div>
        <label class="switch" style="margin-left:auto"><input type="checkbox" id="ep-prompt-toggle"><span></span></label>
      </div>
    </div>

    <div id="ep-catalog"></div>

    <div class="section-card">
      <div class="section-head">
        <span class="material-symbols-outlined">terminal</span>
        <div><h3>${esc(ew('vscodeAlias', 'VS Code token alias'))}</h3>
          <div class="muted small" id="ep-vscode-note">—</div></div>
        <span class="tag" style="margin-left:auto">CLI</span>
      </div>
    </div>`;
  },
  after: async () => {
    const cw = (k, fb) => T('common.' + k) || fb;
    const ew = (k, fb) => T('endpoints.' + k) || fb;
    const v = await api('/v1/endpoints').catch(() => null);
    if (!v) { $('ep-catalog').innerHTML = '<div class="na-note">endpoint data unavailable</div>'; return; }
    $('ep-base').textContent = v.active.public;
    $('ep-public').textContent = v.active.public;
    $('ep-local').textContent = v.active.local;
    $('ep-sid').textContent = '· ' + v.localServer.id.slice(0, 8);
    $('ep-surl').textContent = v.localServer.url;
    $('ep-test').href = v.active.public + '/models';
    document.querySelectorAll('[data-copy-el]').forEach((b) => b.addEventListener('click', async (e) => {
      e.preventDefault();
      await navigator.clipboard.writeText($(b.dataset.copyEl).textContent);
      toast(cw('copied', 'copied'));
    }));

    const tunnels = v.tunnels || [];
    $('ep-tunnel-count').textContent = `${tunnels.filter((t) => t.state === 'enabled').length} / ${tunnels.length} ${ew('activeShort', 'active')}`;
    $('ep-tunnels').innerHTML = tunnels.map((t) => `
      <div class="endpoint-row">
        <span class="material-symbols-outlined">cloud</span><b>${esc(t.label)}</b>
        <span class="tag ${t.state === 'needs-auth' ? 'warn' : ''}" style="margin-left:auto">${esc(t.state)}</span>
        <button class="grad-btn" disabled title="${esc(t.reason)}">${esc(ew('enable', 'Enable'))}</button>
      </div>
      <div class="muted small" style="margin:-4px 0 8px 34px">${esc(t.reason)}</div>`).join('');

    const prompt = v.customSystemPrompt;
    $('ep-prompt-toggle').checked = !!prompt;
    $('ep-prompt-toggle').addEventListener('change', async () => {
      let value = '';
      if ($('ep-prompt-toggle').checked) {
        value = window.prompt(ew('customPromptDesc', 'Custom system prompt'), prompt || '') || '';
        if (!value.trim()) { $('ep-prompt-toggle').checked = false; return; }
      }
      await api('/v1/settings/custom-system-prompt', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ prompt: value }) });
      toast(value ? ew('promptSaved', 'system prompt saved') : ew('promptCleared', 'system prompt cleared'));
    });

    const GROUPS = [
      ['chat', ew('groupChat', 'Chat'), ['chat', 'responses', 'completions', 'messages']],
      ['media', ew('groupMedia', 'Media & multimodal'), ['embeddings', 'images-generations', 'images-edits', 'audio-transcriptions', 'audio-speech', 'music-generations', 'videos-generations']],
      ['search', ew('groupSearch', 'Search & discovery'), ['search']],
      ['tools', ew('groupTools', 'Tools & management'), ['rerank', 'moderations', 'batches', 'files', 'models-list']],
    ];
    const byId = Object.fromEntries((v.endpoints || []).map((e) => [e.id, e]));
    $('ep-catalog').innerHTML = GROUPS.map(([gid, label, ids]) => `
      <div class="section-title"><h3>${esc(label)}</h3></div>
      <div class="card-grid4">
        ${ids.filter((id) => byId[id]).map((id) => {
          const e = byId[id];
          return `<div class="pcard2">
            <div class="pcard2-head">
              <span class="plogo2"><span class="material-symbols-outlined" style="color:${iconAccent(e.id)}">${e.kind === 'models' ? 'list' : 'api'}</span></span>
              <div class="pcard2-name">${esc(e.id.replace(/-/g, ' '))}${e.kind === 'chat' && e.id === 'messages' ? ' <span class="tag info">ANTHROPIC</span>' : ''}</div>
            </div>
            <div class="muted small">${e.models} ${cw('models', 'models')}</div>
            <div class="pcard2-foot"><code class="muted small">${esc(e.path)}</code>
              <button class="icon-btn" data-copy-path="${esc(e.path)}"><span class="material-symbols-outlined">content_copy</span></button></div>
          </div>`;
        }).join('')}
      </div>`).join('');
    document.querySelectorAll('[data-copy-path]').forEach((b) => b.addEventListener('click', async () => {
      await navigator.clipboard.writeText($('ep-base').textContent.replace('/v1', '') + b.dataset.copyPath);
      toast(cw('copied', 'copied'));
    }));

    $('ep-vscode-note').textContent = v.vscodeAlias.implemented
      ? ew('vscodeImplemented', 'alias active') : (v.vscodeAlias.reason || '');

    const protoNotes = {
      openai: '',
      mcp: ew('mcpNote', 'The MCP stdio engine is not part of the Rust build.'),
      a2a: ew('a2aNote', 'A2A agent endpoints are not part of the Rust build.'),
      context: ew('contextNote', 'Context sources (file/memory providers) are not part of the Rust build.'),
    };
    $('ep-protocols').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => {
      $('ep-protocols').querySelectorAll('button').forEach((x) => x.classList.remove('active'));
      b.classList.add('active');
      const note = protoNotes[b.dataset.p] || '';
      $('ep-proto-note').style.display = note ? 'block' : 'none';
      $('ep-proto-note').textContent = note;
    }));
  },
};

PAGES.apikeys = {
  title: 'API Manager',
  body: () => {
    const cw = (k, fb) => T('common.' + k) || fb;
    const aw = (k, fb) => T('apiManager.' + k) || fb;
    return `
    <h1>${esc(aw('title', 'API Key management'))}</h1>
    <p class="muted small">${esc(aw('subtitle', 'Create and manage the API keys used to reach your endpoints'))}</p>
    <div class="apikey-actions"><button class="grad-btn" id="key-new">+ ${esc(aw('create', 'Create API key'))}</button></div>
    <div class="flow">${esc(aw('yourApp', 'Your app'))} <span class="material-symbols-outlined">arrow_forward</span>
      <b>API key</b> <span class="material-symbols-outlined">arrow_forward</span> <b>OmniRoute</b></div>

    <div class="filter-card">
      <div class="filter-row">
        <div class="search-wrap"><span class="material-symbols-outlined">search</span>
          <input type="search" id="key-q" placeholder="${esc(cw('search', 'Search'))}…" autocomplete="off"></div>
        <label class="switch"><input type="checkbox" id="key-enabled-only"><span></span>${esc(aw('activeOnly', 'Enabled only'))}</label>
      </div>
      <div class="chip-row" id="key-status-chips"></div>
      <div class="chip-row" id="key-type-chips"></div>
    </div>

    <div class="section-card">
      <div class="section-head">
        <span class="material-symbols-outlined" style="color:var(--color-primary)">vpn_key</span>
        <div><h3 id="keys-count">—</h3><div class="muted small">${esc(aw('registeredHint', 'Each key tracks its own usage and can be revoked independently. For safety the secret is masked after creation.'))}</div></div>
        <button class="grad-btn" id="key-new-2">+ ${esc(aw('create', 'Create API key'))}</button>
      </div>
      <h4 class="sub-head"><span class="material-symbols-outlined">key</span> <span id="keys-subhead">—</span></h4>
      <table class="keys-table">
        <thead><tr>
          <th>${esc(cw('name', 'Name'))}</th><th>${esc(aw('keyColumn', 'Key'))}</th>
          <th>${esc(aw('permissions', 'Permissions'))}</th><th>${esc(aw('usage', 'Usage'))}</th>
          <th>${esc(cw('created', 'Created'))}</th><th>${esc(cw('actions', 'Actions'))}</th>
        </tr></thead>
        <tbody id="key-rows"></tbody>
      </table>
    </div>`;
  },
  after: async () => {
    const cw = (k, fb) => T('common.' + k) || fb;
    const aw = (k, fb) => T('apiManager.' + k) || fb;
    let all = [];
    let status = 'all';
    let type = 'all';

    const load = async () => {
      const v = await api('/v1/api-keys');
      all = v.api_keys || [];
      draw();
    };
    const cnt = (pred) => all.filter(pred).length;
    const draw = () => {
      const q = ($('key-q').value || '').toLowerCase();
      const onlyEnabled = $('key-enabled-only').checked;
      const rows = all.filter((k) => {
        if (q && !(k.name.toLowerCase().includes(q) || (k.key || '').toLowerCase().includes(q))) return false;
        if (onlyEnabled && k.status !== 'enabled') return false;
        if (status !== 'all' && k.status !== status) return false;
        if (type !== 'all' && (k.type || 'standard') !== type) return false;
        return true;
      });
      const chips = (el, items, active, onPick) => {
        $(el).innerHTML = items.map(([id, label, n]) =>
          `<button class="chip ${id === active ? 'active' : ''}" data-chip="${id}">${esc(label)} <b>${n}</b></button>`).join('');
        $(el).querySelectorAll('button').forEach((b) => b.addEventListener('click', () => { onPick(b.dataset.chip); }));
      };
      chips('key-status-chips', [
        ['all', cw('all', 'All'), all.length],
        ['enabled', aw('statusEnabled', 'Enabled'), cnt((k) => k.status === 'enabled')],
        ['disabled', aw('statusDisabled', 'Disabled'), cnt((k) => k.status === 'disabled')],
        ['revoked', aw('statusRevoked', 'Revoked'), cnt((k) => k.status === 'revoked' || k.status === 'banned')],
        ['expired', aw('statusExpired', 'Expired'), cnt((k) => k.status === 'expired')],
      ], status, (v) => { status = v; draw(); });
      chips('key-type-chips', [
        ['all', cw('all', 'All'), all.length],
        ['standard', aw('typeStandard', 'Standard'), cnt((k) => (k.type || 'standard') === 'standard')],
        ['admin', aw('typeAdmin', 'Admin'), cnt((k) => k.type === 'admin')],
        ['restricted', aw('typeRestricted', 'Restricted'), cnt((k) => k.type === 'restricted')],
      ], type, (v) => { type = v; draw(); });

      $('keys-count').textContent = `${aw('registeredKeys', 'Registered keys')} (${all.length})`;
      const std = all.filter((k) => (k.type || 'standard') === 'standard').length;
      const adm = all.filter((k) => k.type === 'admin').length;
      $('keys-subhead').textContent = `${aw('standardKeys', 'Standard keys')} ${std}` + (adm ? ` · ${aw('adminKeys', 'Admin keys')} ${adm}` : '');

      $('key-rows').innerHTML = rows.length ? rows.map((k) => {
        const created = new Date(k.created_at_ms || Date.now()).toLocaleDateString();
        const used = k.last_used_at_ms ? new Date(k.last_used_at_ms).toLocaleDateString() : aw('neverUsed', 'never used');
        const perm = k.modelAccessMode === 'restricted' && (k.allowedModels || []).length
          ? `${(k.allowedModels || []).length} ${cw('models', 'models')}`
          : aw('allModels', 'All models');
        return `<tr>
          <td><span class="material-symbols-outlined" style="font-size:15px;color:${iconAccent(k.id)}">key</span> ${esc(k.name)}</td>
          <td><code class="masked">${esc(k.key)}</code> <span class="material-symbols-outlined lock">lock</span></td>
          <td><span class="perm-badge"><span class="material-symbols-outlined">lock_open</span>${esc(perm)}</span></td>
          <td><b>${k.total_requests || 0}</b> <span class="muted small">${cw('requests', 'requests')}</span>
              <div class="muted small">US$ ${(k.cost_usd || 0).toFixed(4)}</div>
              <div class="muted small">${esc(used)}</div></td>
          <td class="muted small">${created}</td>
          <td class="row-actions">
            <button class="icon-btn" data-act="copy" data-id="${k.id}" title="copy"><span class="material-symbols-outlined">content_copy</span></button>
            <button class="icon-btn" data-act="rotate" data-id="${k.id}" title="rotate"><span class="material-symbols-outlined">refresh</span></button>
            <button class="icon-btn" data-act="toggle" data-id="${k.id}" data-on="${k.enabled}" title="enable/disable"><span class="material-symbols-outlined">${k.enabled ? 'toggle_on' : 'toggle_off'}</span></button>
            <button class="icon-btn danger" data-act="revoke" data-id="${k.id}" title="revoke"><span class="material-symbols-outlined">delete</span></button>
          </td></tr>`;
      }).join('') : `<tr><td colspan="6" class="muted small">${esc(cw('noData', 'no data'))}</td></tr>`;

      $('key-rows').querySelectorAll('button[data-act]').forEach((b) => b.addEventListener('click', async () => {
        const id = b.dataset.id;
        const act = b.dataset.act;
        if (act === 'copy') { await navigator.clipboard.writeText(id); toast('id copied'); return; }
        if (act === 'rotate') {
          if (!confirm(aw('rotateConfirm', 'Issue a new secret for this key?'))) return;
          const v = await api('/v1/api-keys/' + id + '/rotate', { method: 'POST' });
          prompt('new secret (copy now — it cannot be shown again):', v.api_key.key);
          load(); return;
        }
        if (act === 'toggle') {
          await api('/v1/api-keys/' + id, { method: 'PATCH', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ enabled: !(b.dataset.on === 'true') }) });
          load(); return;
        }
        if (act === 'revoke') {
          if (!confirm(aw('revokeConfirm', 'Revoke this key permanently?'))) return;
          await api('/v1/api-keys/' + id, { method: 'DELETE' });
          load(); return;
        }
      }));
    };

    const createKey = async () => {
      const name = prompt(aw('keyNamePrompt', 'key name:') || 'key name:');
      if (!name) return;
      const type = prompt(aw('keyTypePrompt', 'type standard|admin|restricted:') || 'type standard|admin|restricted:', 'standard') || 'standard';
      const v = await api('/v1/api-keys', {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          name, role: type === 'admin' ? 'admin' : 'default', type,
          modelAccessMode: type === 'restricted' ? 'restricted' : 'all',
        }),
      });
      prompt(aw('copyNow', 'copy the secret now — it is shown only once:'), v.api_key.key);
      load();
    };
    $('key-q').addEventListener('input', draw);
    $('key-enabled-only').addEventListener('change', draw);
    $('key-new').onclick = createKey;
    $('key-new-2').onclick = createKey;
    await load();
  },
};

PAGES.providers = {
  title: 'Providers',
  body: () => {
    // Parity: the original's providers namespace (NOT sidebar.*).
    const pw = (k, fb) => T('providers.' + k) || fb;
    const cw = (k, fb) => T('common.' + k) || fb;
    return `
    <h1>${esc(pw('title', 'Providers'))}</h1>
    <p class="muted small">${esc(pw('providersSubtitle', 'Manage AI provider connections'))}</p>

    <div id="pv-hint"></div>
    <div id="pv-expiry"></div>
    <div class="filter-card">
      <div class="filter-row">
        <div class="search-wrap"><span class="material-symbols-outlined">search</span>
          <input type="search" id="pv-q" placeholder="${esc(pw('searchProviders', 'Search providers'))}" autocomplete="off"></div>
        <div class="search-wrap"><span class="material-symbols-outlined">psychology</span>
          <input type="search" id="pv-qm" placeholder="${esc(pw('searchByModel', 'Search by model…'))}" autocomplete="off"></div>
        <div class="segmented" id="pv-mode">
          <button data-mode="all" class="active" title="${esc(pw('providerDisplayModeAllDesc', 'Show every provider in grouped sections.'))}"><span class="material-symbols-outlined">view_module</span>${esc(pw('providerDisplayModeAll', 'All'))}</button>
          <button data-mode="configured" title="${esc(pw('providerDisplayModeConfiguredDesc', 'Show providers with saved connections.'))}"><span class="material-symbols-outlined">check_circle</span>${esc(pw('providerDisplayModeConfigured', 'Configured'))}</button>
          <button data-mode="compact" title="${esc(pw('providerDisplayModeCompactDesc', 'Show configured and no-auth providers once in a single flat list.'))}"><span class="material-symbols-outlined">view_agenda</span>${esc(pw('providerDisplayModeCompact', 'Compact'))}</button>
        </div>
        <button class="grad-btn" id="pv-new">+ ${esc(pw('onboardingWizardShort', 'Onboarding Wizard'))}</button>
        <button class="mini" id="pv-import-file"><span class="material-symbols-outlined" style="font-size:15px;vertical-align:-3px">upload_file</span> ${esc(pw('importFromFile', 'Import from file'))}</button>
        <button class="mini" id="pv-test-all"><span class="material-symbols-outlined" style="font-size:15px;vertical-align:-3px">play_arrow</span> ${esc(pw('testAll', 'Test all'))}</button>
      </div>
      <div class="chip-row" id="pv-cats"></div>
      <div class="chip-row" id="pv-media"></div>
    </div>
    <div id="pv-sections"><span class="muted">loading…</span></div>
    <div id="pv-results"></div>`;
  },
  after: async () => {
    // Parity: providers namespace + the original's filter state
    // (activeCategory / showFreeOnly / activeServiceKind / displayMode).
    const pw = (k, fb) => T('providers.' + k) || fb;
    const cw = (k, fb) => T('common.' + k) || fb;
    let catalog = [];
    let connections = [];
    let compatibleNodes = [];
    let mode = 'all';
    let cat = null;
    let freeOnly = false;
    let mediaKind = null;
    let testingMode = null;
    let lastHighlight = null;
    try { lastHighlight = window.history.state && window.history.state.providerId; } catch {}

    // Section catalogue mirroring the original page order (compatible →
    // oauth → ide → webcookie → free → apikey/LLM → no-auth →
    // upstream-proxy → webfetch → aggregators → enterprise → cloud-agent →
    // local → search → embedding → image → audio → video).
    const SECTIONS = [
      ['compatible', '#f97316', pw('compatibleProviders', 'Compatible providers'), pw('compatibleProvidersDesc', 'OpenAI / Anthropic compatible endpoints you added yourself.')],
      ['oauth', '#3b82f6', pw('oauthProviders', 'OAuth providers'), pw('oauthProvidersDesc', 'Providers authenticated over OAuth — sign in once and OmniRoute handles token rotation.')],
      ['ide', '#06b6d4', pw('ideProviders', 'IDE Providers'), pw('ideProvidersDesc', "Editors with built-in AI subscription. Use the provider page to import credentials directly from the IDE's keychain.")],
      ['webcookie', '#a855f7', pw('webCookieProviders', 'Web / Cookie providers'), pw('webCookieProvidersDesc', 'These providers use browser web sessions, cookies, or web tokens instead of API keys. Open a provider to add the required session credential.')],
      ['free', '#22c55e', pw('freeTierProviders', 'Free tier providers'), pw('freeAggregated', 'Providers with a free tier, aggregated across every section.')],
      ['apikey', '#f59e0b', pw('apiKeyProviders', 'API key providers'), pw('apiKeyProvidersDesc', 'Standard API-key providers. Add a key and OmniRoute routes, retries and rate-limits for you.')],
      ['no-auth', '#78716c', pw('noAuthProviders', 'No-auth providers'), pw('noAuthProvidersDesc', 'Providers that need no credentials.')],
      ['upstream-proxy', '#6366f1', pw('upstreamProxyProviders', 'Upstream proxy providers'), pw('upstreamProxyProvidersDesc', 'Requests tunnelled through an upstream proxy.')],
      ['webfetch', '#f97316', pw('webFetchProvidersHeading', 'Web fetch providers'), pw('webFetchProvidersDesc', 'Providers that can fetch web content.')],
      ['aggregators', '#f59e0b', pw('aggregatorsGateways', 'Aggregators & gateways'), pw('aggregatorsGatewaysDesc', 'Multi-provider aggregators and AI gateways that expose dozens of underlying models behind one unified API.')],
      ['enterprise', '#f59e0b', pw('enterpriseCloud', 'Enterprise & cloud'), pw('enterpriseCloudDesc', 'Enterprise clouds (Azure, Bedrock, Vertex, …).')],
      ['cloudagent', '#8b5cf6', pw('cloudAgentProviders', 'Cloud agent providers'), pw('cloudAgentProvidersDesc', 'Hosted agent runtimes.')],
      ['local', '#10b981', pw('localProviders', 'Local / self-hosted providers'), pw('localProvidersDesc', 'Models served from your own machine.')],
      ['search', '#14b8a6', pw('searchProvidersHeading', 'Search providers'), pw('searchProvidersDesc', 'Web search and fetch providers.')],
      ['embedding', '#f59e0b', pw('embeddingRerankProviders', 'Embeddings & rerank'), pw('embeddingRerankProvidersDesc', 'Embedding and rerank providers.')],
      ['image', '#f59e0b', pw('imageProviders', 'Image providers'), pw('imageProvidersDesc', 'Image generation providers.')],
      ['audio', '#f43f5e', pw('audioProvidersHeading', 'Audio providers'), pw('audioProvidersDesc', 'Speech-to-text and text-to-speech providers.')],
      ['video', '#f59e0b', pw('videoProviders', 'Video providers'), pw('videoProvidersDesc', 'Video generation providers.')],
    ];
    const CATS = [
      [null, null, pw('providerSummaryAll', 'All')],
      ['oauth', '#3b82f6', pw('oauthLabel', 'OAuth')],
      ['ide', '#06b6d4', pw('categoryIde', 'IDE')],
      ['free', '#22c55e', pw('freeTier', 'Free Tier')],
      ['no-auth', '#78716c', pw('noAuthLabel', 'No Auth')],
      ['upstream-proxy', '#6366f1', pw('upstreamProxyLabel', 'Upstream proxy')],
      ['apikey', '#f59e0b', pw('apiKeyLabel', 'API key')],
      ['compatible', '#f97316', pw('compatibleLabel', 'Compatible')],
      ['webcookie', '#a855f7', pw('categoryWebCookie', 'Web Cookie')],
      ['search', '#14b8a6', pw('categorySearch', 'Search')],
      ['webfetch', '#f97316', pw('webFetch', 'Web fetch')],
      ['audio', '#f43f5e', pw('categoryAudio', 'Audio')],
      ['local', '#10b981', pw('categoryLocal', 'Local')],
      ['cloudagent', '#8b5cf6', pw('categoryCloudAgent', 'Cloud Agent')],
    ];
    const MEDIA_CHIPS = [
      ['image', 'image', pw('serviceKindImage', 'Image')],
      ['video', 'videocam', pw('serviceKindVideo', 'Video')],
      ['music', 'music_note', pw('serviceKindMusic', 'Music')],
      ['tts', 'record_voice_over', pw('serviceKindTts', 'Text→Speech')],
      ['stt', 'hearing', pw('serviceKindStt', 'Speech→Text')],
      ['embedding', 'scatter_plot', pw('serviceKindEmbedding', 'Embedding')],
    ];
    // Provider → section membership (parity: the original's ID-set
    // partitioning in page.tsx + shouldShowProviderSection).
    const secOf = (p) => {
      if (p.ide) return 'ide';
      if (p.category === 'oauth') return 'oauth';
      if (p.category === 'web-cookie') return 'webcookie';
      if (p.category === 'noauth') return 'no-auth';
      if (p.category === 'upstream-proxy') return 'upstream-proxy';
      if (p.category === 'cloud-agent') return 'cloudagent';
      if (p.category === 'local') return 'local';
      if (p.category === 'search') return 'search';
      if (p.category === 'audio') return 'audio';
      if (p.aggregator) return 'aggregators';
      if (p.enterprise) return 'enterprise';
      if (p.imageOnly) return 'image';
      if (p.videoGen) return 'video';
      if (p.embeddingRerank) return 'embedding';
      return 'apikey';
    };
    const inSection = (p, sec) => {
      if (sec === 'free') return !!p.freeTier;
      if (sec === 'webfetch') return (p.serviceKinds || []).includes('webFetch');
      return secOf(p) === sec;
    };
    const showSection = (sec) => {
      if (freeOnly) return sec === 'free';
      if (cat) return cat === sec;
      return sec !== 'free' && sec !== 'webfetch';
    };
    // Parity: ProviderCard isLlmProvider predicate.
    const isLlm = (p, authType) => {
      const kinds = p.serviceKinds || [];
      return kinds.includes('llm')
        || (kinds.length === 0 && !['search', 'audio', 'cloud-agent', 'cloudagent', 'upstream-proxy', 'no-auth', 'noauth'].includes(authType || secOf(p)));
    };
    const kindLabel = (k) => ({
      llm: pw('serviceKindChat', 'Chat'), embedding: pw('serviceKindEmbedding', 'Embed'),
      image: pw('serviceKindImage', 'Image'), imageToText: pw('serviceKindImageToText', 'I→T'),
      tts: pw('serviceKindTts', 'TTS'), stt: pw('serviceKindStt', 'STT'),
      webSearch: pw('serviceKindWebSearch', 'Search'), webFetch: pw('serviceKindWebFetch', 'Fetch'),
      video: pw('serviceKindVideo', 'Video'), music: pw('serviceKindMusic', 'Music'),
    }[k] || k);
    const mediaHit = (p, mk) => {
      const kinds = p.serviceKinds || [];
      if (mk === 'image') return kinds.includes('image') || kinds.includes('imageToText');
      if (mk === 'embedding') return kinds.includes('embedding');
      return kinds.includes(mk);
    };
    const syncUrl = () => {
      try {
        const u = new URL(location.href);
        const setOrDel = (k, v) => { if (v) u.searchParams.set(k, v); else u.searchParams.delete(k); };
        setOrDel('search', ($('pv-q').value || '').trim());
        setOrDel('model', ($('pv-qm').value || '').trim());
        setOrDel('mode', mode !== 'all' ? mode : null);
        setOrDel('cat', freeOnly ? 'free' : cat);
        setOrDel('media', mediaKind);
        history.replaceState(history.state, '', u.toString());
      } catch {}
    };

    // Parity: ProviderCard rows — identity (icon + name + risk + category
    // dot) / capabilities (service-kind chips + compatible badges) / footer
    // (connection status + toggle + test). The whole card opens the detail
    // view (parity: /dashboard/providers/[id]).
    const DOT_OF = { compatible: '#f97316', oauth: '#3b82f6', apikey: '#f59e0b', 'no-auth': '#78716c', 'web-cookie': '#a855f7', search: '#14b8a6', audio: '#f43f5e', local: '#10b981', 'upstream-proxy': '#6366f1', 'cloud-agent': '#8b5cf6' };
    const card = (p, authType) => {
      const st = p.stats || { total: 0, connected: 0, error: 0 };
      const total = Number(st.total || 0);
      const conn = Number(st.connected || 0);
      const err = Number(st.error || 0);
      const allDisabled = !!st.allDisabled;
      const at = authType || secOf(p);
      const kinds = (p.serviceKinds || []);
      const llm = isLlm(p, at);
      const hl = lastHighlight === p.id ? ' id="pv-highlight" style="outline:2px solid var(--color-primary)"' : '';
      const status = allDisabled
        ? `<span class="tag">${esc(pw('disabled', 'disabled'))}</span>`
        : total === 0
          ? `<span class="muted small">${esc(pw('noConnections', 'No connections'))}</span>`
          : `${conn > 0 ? `<span class="tag" style="border-color:rgba(34,197,94,.5);color:var(--ok)">${esc(pw('connected', 'connected {count}').replace('{count}', conn))}</span>` : ''}`
            + `${err > 0 ? `<span class="tag warn">${esc(pw('errorCountNoCode', '{count} error').replace('{count}', err))}</span>` : ''}`
            + `${p.cooldownMs > 0 ? `<span class="tag warn">${esc(pw('cooldown', 'cooldown'))}</span>` : ''}`;
      const kindChips = kinds.slice(0, 4).map((k) => `<span class="tag info">${esc(kindLabel(k))}</span>`).join('');
      return `<div class="pcard2 ${conn > 0 && !allDisabled ? 'connected' : ''} ${p.risk ? 'risky' : ''}" data-card="${esc(p.id)}"${hl}>
        <div class="pcard2-head">
          <span class="plogo2" style="color:${esc(p.color || '#888')};background:${esc(p.color || '#888')}15;border-radius:8px;width:34px;height:34px">
            <span class="material-symbols-outlined">${esc(p.icon || 'cloud')}</span></span>
          <div class="pcard2-name" title="${esc(p.name)}">${esc(p.name)}</div>
          <div class="pcard2-dots">
            ${p.risk ? `<button class="icon-btn" data-risk="${esc(p.id)}" title="${esc(pw('riskNotice.tooltip', 'Usage caveats'))}" style="color:var(--warn);font-size:14px">info</button>` : ''}
            <span class="cdot" style="background:${DOT_OF[at] || '#f59e0b'}" title="${esc(at)}"></span>
            ${p.freeTier ? '<span class="cdot" style="background:#22c55e" title="free tier"></span>' : ''}
          </div>
        </div>
        <div class="pcard2-tags">${p.freeTier ? `<span class="tag">${esc(cw('free', 'free'))}</span>` : ''}${kindChips}</div>
        <div class="pcard2-foot">
          <span class="small" style="display:flex;gap:6px;align-items:center;flex-wrap:wrap">${status}</span>
          <span style="margin-left:auto;display:flex;gap:6px;align-items:center">
          ${total > 0 ? `<label class="switch mini-switch" title="${esc(allDisabled ? pw('enableProvider', 'Enable provider') : pw('disableProvider', 'Disable provider'))}"><input type="checkbox" data-toggle="${esc(p.id)}" ${allDisabled ? '' : 'checked'}><span></span></label>` : ''}
          ${llm ? `<button class="mini" data-test="${esc(p.id)}"><span class="material-symbols-outlined" style="font-size:14px;vertical-align:-3px">play_arrow</span> ${esc(pw('testConnection', 'Test'))}</button>` : '<span class="material-symbols-outlined muted">chevron_right</span>'}
          </span>
        </div>
      </div>`;
    };
    const compatCard = (n) => {
      const llmKind = (n.apiType || 'chat').includes('audio') || (n.apiType || '').includes('images') || (n.apiType || '') === 'embeddings' ? false : true;
      return `<div class="pcard2 ${n.enabled ? 'connected' : ''}" data-card="${esc(n.id)}">
        <div class="pcard2-head">
          <span class="plogo2" style="color:#10A37F;background:#10A37F15;border-radius:8px;width:34px;height:34px"><span class="material-symbols-outlined">extension</span></span>
          <div class="pcard2-name" title="${esc(n.name)}">${esc(n.name)}</div>
          <div class="pcard2-dots"><span class="cdot" style="background:#f97316" title="compatible"></span></div>
        </div>
        <div class="pcard2-tags"><span class="tag info">${esc(n.kind === 'claudeCode' ? 'CC' : n.kind === 'anthropic' ? pw('messages', 'Messages') : pw('chat', 'Chat'))}</span>
          <span class="tag info">${esc(n.id.slice(0, 24))}</span></div>
        <div class="pcard2-foot">
          <span class="small ${n.enabled ? 's-ok' : 'muted'}">${n.enabled ? esc(pw('connected', 'connected {count}').replace('{count}', 1)) : esc(pw('disabled', 'disabled'))}</span>
          <span style="margin-left:auto;display:flex;gap:6px;align-items:center">
            <label class="switch mini-switch"><input type="checkbox" data-compat-toggle="${esc(n.id)}" ${n.enabled ? 'checked' : ''}><span></span></label>
            ${llmKind ? `<button class="mini" data-test="${esc(n.id)}"><span class="material-symbols-outlined" style="font-size:14px;vertical-align:-3px">play_arrow</span> ${esc(pw('testConnection', 'Test'))}</button>` : ''}
          </span>
        </div>
      </div>`;
    };

    const matchQ = (p) => {
      const q = ($('pv-q').value || '').toLowerCase().trim();
      const qm = ($('pv-qm').value || '').toLowerCase().trim();
      if (q && !(p.name + ' ' + p.id + ' ' + (p.alias || '')).toLowerCase().includes(q)) return false;
      if (qm) {
        const models = (p.models || []).join(' ').toLowerCase();
        const kinds = (p.serviceKinds || []).join(' ').toLowerCase();
        if (!models.includes(qm) && !kinds.includes(qm) && !p.id.includes(qm)) return false;
      }
      if (mediaKind && !mediaHit(p, mediaKind)) return false;
      return true;
    };
    const cfgOnly = (p) => mode !== 'configured' || Number(((p.stats || {}).total) || 0) > 0;
    const passFilters = (p) => cfgOnly(p) && matchQ(p);
    const countCfg = (list) => ({
      configured: list.filter((p) => Number(((p.stats || {}).total) || 0) > 0).length,
      total: list.length,
    });

    // ── batch test (parity: handleBatchTest + ProviderTestResultsView) ──
    const batchTest = async (testMode, providerId) => {
      if (testingMode) return;
      testingMode = testMode === 'provider' ? providerId : testMode;
      draw();
      try {
        const data = await api('/v1/providers/test-batch', {
          method: 'POST', headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ mode: testMode, providerId: providerId || undefined }),
        });
        showTestResults(data);
        const s = data.summary || {};
        if ((s.failed || 0) === 0) toast(pw('allTestsPassed', 'All {total} tests passed').replace('{total}', s.total || 0));
        else toast(pw('testSummary', '{passed} passed, {failed} failed of {total}').replace('{passed}', s.passed || 0).replace('{failed}', s.failed || 0).replace('{total}', s.total || 0), false);
      } catch { toast(pw('providerTestFailed', 'Provider test failed'), false); }
      testingMode = null;
      draw();
    };
    const showTestResults = (results) => {
      const items = Array.isArray(results.results) ? results.results : [];
      const s = results.summary || {};
      const modeLabel = { oauth: pw('oauthLabel', 'OAuth'), free: cw('free', 'Free'), apikey: pw('apiKeyLabel', 'API key'), compatible: pw('compatibleLabel', 'Compatible'), provider: pw('providerLabel', 'Provider'), all: cw('all', 'All') }[results.mode] || results.mode || '';
      $('modal-card').innerHTML = `
        <h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main)">${esc(pw('testResults', 'Test results'))}</h2>
        <div class="muted small" style="margin-bottom:10px">${esc(pw('modeTest', 'Mode: {mode}').replace('{mode}', modeLabel))} ·
          <b style="color:var(--ok)">${esc(pw('passedCount', '{count} passed').replace('{count}', s.passed || 0))}</b>
          ${(s.failed || 0) > 0 ? ` · <b style="color:var(--bad)">${esc(pw('failedCount', '{count} failed').replace('{count}', s.failed))}</b>` : ''}
          · ${esc(pw('testedCount', '{count} tested').replace('{count}', s.total || 0))}</div>
        ${items.length ? items.map((r) => `
          <div class="endpoint-row">
            <span class="material-symbols-outlined" style="color:${r.valid ? 'var(--ok)' : 'var(--bad)'}">${r.valid ? 'check_circle' : 'error'}</span>
            <div style="flex:1;min-width:0"><b>${esc(r.connectionName || r.provider)}</b> <span class="muted small">(${esc(r.provider)})</span>
              ${r.valid ? '' : `<div class="muted small">${esc((r.error || '').slice(0, 160))}</div>`}</div>
            ${r.latencyMs ? `<span class="muted small">${r.latencyMs}ms</span>` : ''}
            <span class="tag ${r.valid ? '' : 'warn'}">${esc(r.valid ? pw('okShort', 'OK') : ((r.diagnosis && r.diagnosis.type) || pw('errorShort', 'ERR')))}</span>
          </div>`).join('') : `<div class="na-note">${esc(pw('noActiveConnectionsInGroup', 'No active connections in this group.'))}</div>`}
        <div style="margin-top:12px;text-align:right"><button class="mini" id="pv-results-close">${esc(cw('close', 'Close'))}</button></div>`;
      $('modal').style.display = 'flex';
      $('pv-results-close').addEventListener('click', () => { $('modal').style.display = 'none'; });
    };

    // ── provider detail (parity: /dashboard/providers/[id] page) ──
    const openDetail = (pid) => {
      try { history.replaceState({ ...history.state, providerId: pid }, ''); } catch {}
      const p = catalog.find((x) => x.id === pid);
      const node = compatibleNodes.find((x) => x.id === pid);
      const entry = p || (node ? { id: node.id, name: node.name, icon: 'extension', color: '#10A37F', serviceKinds: [], models: node.models || [], category: 'compatible', website: null, freeTier: false, risk: false, stats: node.stats } : null);
      if (!entry) return;
      const conns = connections.filter((c) => c.provider === pid);
      const models = entry.models && entry.models.length ? entry.models : (node ? node.models || [] : []);
      $('modal-card').innerHTML = `
        <h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main);display:flex;align-items:center;gap:8px">
          <span class="material-symbols-outlined" style="color:${esc(entry.color || '#888')}">${esc(entry.icon || 'cloud')}</span>${esc(entry.name)}</h2>
        <p class="muted small">${esc(pid)}${entry.website ? ` · <a href="${esc(entry.website)}" target="_blank" rel="noreferrer">${esc(entry.website.replace('https://', '').split('/')[0])}</a>` : ''}</p>
        ${entry.freeTier ? `<p><span class="tag">${esc(cw('free', 'free'))}</span> <span class="muted small">${esc(entry.freeNote || '')}</span></p>` : ''}
        ${entry.risk ? `<div class="na-note">${esc(pw('riskNotice.oauth', 'Check the provider terms before heavy use.'))}</div>` : ''}
        <div class="section-title"><h3>${esc(pw('accounts', 'Accounts'))} <span class="tag">${conns.length}</span></h3>
          <div class="row-actions">${conns.length ? `<button class="mini" id="pv-detail-test"><span class="material-symbols-outlined" style="font-size:14px;vertical-align:-3px">play_arrow</span> ${esc(pw('testAll', 'Test all'))}</button>` : ''}</div></div>
        ${conns.length ? conns.map((c) => `
          <div class="endpoint-row">
            <div style="flex:1;min-width:0"><b>${esc(c.name || c.id)}</b><div class="muted small">${esc(c.id)}${c.baseUrl ? ' · ' + esc(c.baseUrl) : ''}</div></div>
            <label class="switch mini-switch"><input type="checkbox" data-detail-toggle="${esc(c.id)}" ${c.enabled ? 'checked' : ''}><span></span></label>
            <button class="mini" data-detail-test="${esc(c.id)}">${esc(pw('testConnection', 'Test'))}</button>
            <button class="mini" data-detail-del="${esc(c.id)}" style="color:var(--bad)">${esc(cw('delete', 'Delete'))}</button>
          </div>`).join('') : `<div class="na-note">${esc(pw('noProviders', 'No accounts yet — add the first one below.'))}</div>`}
        <div class="section-title"><h3>${esc(pw('newAccount', 'New account'))}</h3></div>
        <div class="filter-row">
          <input id="pv-d-name" placeholder="${esc(pw('accountName', 'Name'))}" style="flex:1;min-width:120px" value="${esc(entry.name)}">
          <input id="pv-d-key" placeholder="API key" style="flex:2;min-width:160px" autocomplete="off">
        </div>
        <div class="filter-row" style="margin-top:8px">
          <input id="pv-d-base" placeholder="Base URL (optional)" style="flex:2;min-width:160px">
          <input id="pv-d-models" placeholder="models (comma separated, optional)" style="flex:2;min-width:160px">
        </div>
        <div style="margin-top:10px;display:flex;gap:8px;justify-content:flex-end">
          <button class="mini" id="pv-detail-close">${esc(cw('close', 'Close'))}</button>
          <button class="grad-btn" id="pv-detail-add">+ ${esc(pw('addProvider', 'Add provider'))}</button>
        </div>
        ${models.length ? `<div class="section-title"><h3>${esc(pw('modelAvailability', 'Models'))} <span class="tag">${models.length}</span></h3></div>
        <p class="muted small">${esc(models.slice(0, 30).join(', '))}${models.length > 30 ? ' …' : ''}</p>` : ''}`;
      $('modal').style.display = 'flex';
      $('pv-detail-close').addEventListener('click', closeDetail);
      $('modal').onclick = (e) => { if (e.target.id === 'modal') closeDetail(); };
      $('modal-card').querySelectorAll('[data-detail-toggle]').forEach((cb) => cb.addEventListener('change', async () => {
        await api('/v1/provider-connections/' + cb.dataset.detailToggle, { method: 'PATCH', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ enabled: cb.checked }) });
        await reloadConnections(); draw(); openDetailRefresh(pid);
      }));
      $('modal-card').querySelectorAll('[data-detail-test]').forEach((b) => b.addEventListener('click', async () => {
        const v = await api('/v1/provider-connections/' + b.dataset.detailTest + '/test', { method: 'POST' }).catch(() => null);
        toast(v && v.ok ? `ok · ${v.latency_ms}ms` : ((v && v.detail) || 'failed'), !!(v && v.ok));
      }));
      $('modal-card').querySelectorAll('[data-detail-del]').forEach((b) => b.addEventListener('click', async () => {
        if (!window.confirm(pw('deleteConfirm', 'Delete this connection?'))) return;
        await api('/v1/provider-connections/' + b.dataset.detailDel, { method: 'DELETE' }).catch(() => null);
        await reloadConnections(); draw(); openDetailRefresh(pid);
      }));
      const dt = $('pv-detail-test');
      if (dt) dt.addEventListener('click', () => batchTest('provider', pid));
      $('pv-detail-add').addEventListener('click', async () => {
        const key = $('pv-d-key').value.trim();
        const base = $('pv-d-base').value.trim();
        if (!key && !base && secOf(entry) !== 'no-auth' && secOf(entry) !== 'local' && entry.category !== 'compatible') { toast(pw('apiKeyRequired', 'An API key is required'), false); return; }
        const body = {
          id: 'conn-' + Date.now().toString(36), provider: pid,
          name: $('pv-d-name').value.trim() || entry.name,
          api_key: key || undefined,
          base_url: $('pv-d-base').value.trim() || undefined,
          models: $('pv-d-models').value.split(',').map((s) => s.trim()).filter(Boolean),
          enabled: true,
        };
        const r = await api('/v1/provider-connections', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }).catch(() => null);
        if (!r) { toast(pw('providerTestFailed', 'Provider test failed'), false); return; }
        toast(pw('testSuccess', 'Connection saved'));
        await reloadConnections(); draw(); openDetailRefresh(pid);
      });
    };
    const openDetailRefresh = (pid) => { if ($('modal').style.display === 'flex') openDetail(pid); };
    const closeDetail = () => {
      $('modal').style.display = 'none';
      $('modal').onclick = null;
      lastHighlight = null;
      try { history.replaceState({ ...history.state, providerId: undefined }, ''); } catch {}
    };

    const draw = () => {
      const emptySearch = !(($('pv-q').value || '').trim() || ($('pv-qm').value || '').trim());
      // first-provider hint (parity: addFirstProvider card)
      $('pv-hint').innerHTML = (connections.length === 0 && emptySearch)
        ? `<div class="section-card" style="text-align:center;padding:26px">
            <div style="font-size:32px;color:var(--color-primary)"><span class="material-symbols-outlined" style="font-size:32px">dns</span></div>
            <h2 style="margin:8px 0 4px">${esc(pw('addFirstProvider', 'Add your first provider'))}</h2>
            <p class="muted small" style="max-width:520px;margin:0 auto">${esc(pw('addFirstProviderDesc', 'Connect an AI provider to start routing requests through OmniRoute. You can use free providers, API keys, or OAuth accounts.'))}</p>
            <div style="margin-top:12px;display:flex;gap:8px;justify-content:center;flex-wrap:wrap">
              <button class="grad-btn" id="pv-hint-new">+ ${esc(pw('onboardingWizard', 'Provider Onboarding Wizard'))}</button>
              <a class="mini" href="https://github.com/diegosouzapw/OmniRoute#-documentation" target="_blank" rel="noreferrer">${esc(pw('learnMore', 'Learn more'))}</a>
            </div></div>` : '';
      const hn = $('pv-hint-new');
      if (hn) hn.addEventListener('click', openOnboarding);
      // category chips with configured/total (parity: ProviderSummaryCard)
      $('pv-cats').innerHTML = CATS.map(([id, color, label]) => {
        let pool, extraCfg = 0, extraTotal = 0;
        if (id === null) pool = catalog;
        else if (id === 'free') pool = catalog.filter((p) => p.freeTier);
        else if (id === 'compatible') { pool = []; extraCfg = compatibleNodes.filter((n) => n.enabled).length; extraTotal = compatibleNodes.length; }
        else pool = catalog.filter((p) => inSection(p, id));
        const c = countCfg(pool);
        const cfg = c.configured + extraCfg, total = c.total + extraTotal;
        const active = (id === 'free' && freeOnly) || (!freeOnly && cat === id);
        return `<button class="chip ${active ? 'active' : ''}" data-cat="${id === null ? '' : id}">
          ${color ? `<span class="cdot" style="background:${color}"></span>` : ''}${esc(label)} <b>${cfg}/${total}</b></button>`;
      }).join('');
      $('pv-cats').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => {
        const v = b.dataset.cat || null;
        if (v === 'free') { freeOnly = true; cat = null; }
        else { freeOnly = false; cat = v; }
        syncUrl(); draw();
      }));
      // media chips (parity: SERVICE_KIND_CHIPS + clear)
      $('pv-media').innerHTML = `<span class="muted small" style="align-self:center">${esc(pw('filterByMedia', 'Media'))}</span>` + MEDIA_CHIPS.map(([k, icon, label]) =>
        `<button class="chip ${mediaKind === k ? 'active' : ''}" data-media="${k}"><span class="material-symbols-outlined" style="font-size:14px">${icon}</span>${esc(label)}</button>`).join('')
        + (mediaKind ? `<button class="chip" data-media="">${esc(pw('clearMediaFilter', 'Clear'))}</button>` : '');
      $('pv-media').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => {
        mediaKind = b.dataset.media || null;
        syncUrl(); draw();
      }));
      // display-mode segmented control state
      $('pv-mode').querySelectorAll('button').forEach((x) => x.classList.toggle('active', x.dataset.mode === mode));
      const disCfg = $('pv-mode').querySelector('[data-mode="configured"]');
      if (disCfg) disCfg.style.opacity = connections.length === 0 ? 0.45 : 1;

      if (mode === 'compact') { drawCompact(); bindCardEvents(); return; }
      const out = [];
      // compatible section (dynamic nodes; parity: empty state + add buttons)
      if (showSection('compatible')) {
        const items = compatibleNodes.filter((n) => (mode !== 'configured' || n.enabled) && matchCompat(n));
        out.push(`<div class="section-title"><h3>${esc(pw('compatibleProviders', 'Compatible providers'))}
            <span class="dotmark" style="background:#f97316"></span> <span class="tag">${compatibleNodes.filter((n) => n.enabled).length}/${compatibleNodes.length}</span></h3>
            <div class="row-actions">
              <button class="mini" data-add-compat="openai">+ ${esc(pw('addOpenAICompatible', 'Add OpenAI-compatible'))}</button>
              <button class="mini" data-add-compat="anthropic">+ ${esc(pw('addAnthropicCompatible', 'Add Anthropic-compatible'))}</button>
              <button class="mini" data-add-compat="cc">+ ${esc(pw('addCcCompatible', 'Add CC-compatible'))}</button>
              ${compatibleNodes.length ? `<button class="mini" data-testsec="compatible"><span class="material-symbols-outlined" style="font-size:14px;vertical-align:-3px">play_arrow</span> ${esc(pw('testAll', 'Test all'))}</button>` : ''}
            </div></div>
          <p class="muted small" style="margin:0 0 10px">${esc(pw('compatibleProvidersDesc', 'OpenAI / Anthropic compatible endpoints you added yourself.'))}</p>`);
        out.push(items.length
          ? `<div class="card-grid4">${items.map(compatCard).join('')}</div>`
          : `<div class="na-note">${esc(pw('noCompatibleYet', 'No compatible endpoints yet — add one above.'))}</div>`);
      }
      for (const [secId, secColor, secTitle, secDesc] of SECTIONS) {
        if (secId === 'compatible') continue;
        if (!showSection(secId)) continue;
        const items = catalog.filter((p) => inSection(p, secId)).filter(passFilters);
        const poolAll = catalog.filter((p) => inSection(p, secId));
        // sections with zero catalog entries stay hidden (honest: the Rust
        // catalog has no enterprise/webfetch-only entries of its own)
        if (!poolAll.length && !items.length) continue;
        if (!items.length) continue;
        const c = countCfg(poolAll);
        const llmHead = secId === 'apikey' ? `<div class="subgroup">${esc(pw('llmProviders', 'LLM providers'))}</div>` : '';
        const testBtn = ['oauth', 'ide', 'webcookie', 'free', 'apikey', 'no-auth', 'upstream-proxy', 'cloudagent', 'local', 'search', 'audio'].includes(secId)
          ? `<button class="mini" data-testsec="${secId}"><span class="material-symbols-outlined" style="font-size:14px;vertical-align:-3px">play_arrow</span> ${testingMode === secId ? esc(pw('testing', 'Testing…')) : esc(pw('testAll', 'Test all'))}</button>` : '';
        out.push(`<div class="section-title"><h3>${esc(secTitle)}
            <span class="dotmark" style="background:${secColor}"></span> <span class="tag">${c.configured}/${c.total}</span></h3>
            <div class="row-actions">${testBtn}</div></div>
          <p class="muted small" style="margin:0 0 10px">${esc(secDesc)}</p>${llmHead}
          <div class="card-grid4">${items.map((p) => card(p)).join('')}</div>`);
      }
      $('pv-sections').innerHTML = out.join('') || `<div class="na-note">${esc(pw('noProvidersMatch', 'No providers match your search.'))}</div>`;
      bindCardEvents();
    };
    const matchCompat = (n) => {
      const q = ($('pv-q').value || '').toLowerCase().trim();
      if (q && !(n.name + ' ' + n.id).toLowerCase().includes(q)) return false;
      return true;
    };
    const drawCompact = () => {
      // Parity: compact = single flat list across sections.
      const flat = [
        ...compatibleNodes.filter((n) => (mode !== 'configured' || n.enabled) && matchCompat(n)).map((n) => ({ compat: n })),
        ...catalog.filter(passFilters).filter((p) => freeOnly ? p.freeTier : (!cat || inSection(p, cat))).map((p) => ({ p })),
      ];
      $('pv-sections').innerHTML = flat.length
        ? `<div class="card-grid4 compact">${flat.map((e) => e.compat ? compatCard(e.compat) : card(e.p)).join('')}</div>`
        : `<div class="na-note">${esc(pw('noProvidersMatch', 'No providers match your search.'))}</div>`;
    };

    // ── per-card events, rebound after every draw ──
    const bindCardEvents = () => {
      document.querySelectorAll('[data-card]').forEach((el) => el.addEventListener('click', (e) => {
        if (e.target.closest('button') || e.target.closest('label') || e.target.closest('a') || e.target.closest('input')) return;
        openDetail(el.dataset.card);
      }));
      document.querySelectorAll('[data-risk]').forEach((b) => b.addEventListener('click', (e) => {
        e.stopPropagation();
        $('modal-card').innerHTML = `<h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main)">${esc(pw('riskNotice.detailsTitle', 'Usage caveats'))}</h2>
          <div class="na-note">${esc(pw('riskNotice.oauth', 'This provider has usage caveats — check its terms before heavy use.'))}</div>
          <div style="margin-top:10px;text-align:right"><button class="mini" id="pv-risk-close">${esc(cw('close', 'Close'))}</button></div>`;
        $('modal').style.display = 'flex';
        $('pv-risk-close').addEventListener('click', () => { $('modal').style.display = 'none'; });
      }));
      document.querySelectorAll('[data-test]').forEach((b) => b.addEventListener('click', async (e) => {
        e.stopPropagation();
        const id = b.dataset.test;
        const conn = connections.find((c) => c.provider === id) || connections.find((c) => c.id === id);
        if (!conn) { toast(pw('notConnected', 'provider not connected — add it first'), false); openDetail(id); return; }
        const v = await api('/v1/provider-connections/' + conn.id + '/test', { method: 'POST' }).catch(() => null);
        toast(v && v.ok ? `ok · ${v.latency_ms}ms` : ((v && v.detail) || pw('testFailed', 'Test failed')), !!(v && v.ok));
      }));
      const toggleConns = async (list, on) => {
        await Promise.allSettled(list.map((c) =>
          api('/v1/provider-connections/' + c.id, { method: 'PATCH', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ enabled: on }) }).catch(() => null)));
        toast(on ? cw('enabled', 'enabled') : cw('disabled', 'disabled'));
        await reloadConnections(); draw();
      };
      document.querySelectorAll('[data-toggle]').forEach((cb) => cb.addEventListener('change', (e) => {
        e.stopPropagation();
        toggleConns(connections.filter((c) => c.provider === cb.dataset.toggle), cb.checked);
      }));
      document.querySelectorAll('[data-compat-toggle]').forEach((cb) => cb.addEventListener('change', (e) => {
        e.stopPropagation();
        toggleConns(connections.filter((c) => c.provider === cb.dataset.compatToggle), cb.checked);
      }));
      const secMode = (sec) => ({ webcookie: 'web-cookie', 'no-auth': 'no-auth', 'upstream-proxy': 'upstream-proxy', cloudagent: 'cloud-agent' }[sec] || sec);
      document.querySelectorAll('[data-testsec]').forEach((b) => b.addEventListener('click', () => batchTest(secMode(b.dataset.testsec))));
      document.querySelectorAll('[data-add-compat]').forEach((b) => b.addEventListener('click', () => openCompatModal(b.dataset.addCompat)));
      const hl = $('pv-highlight');
      if (hl && lastHighlight) {
        try { hl.scrollIntoView({ block: 'center' }); } catch {}
        setTimeout(() => { const h = $('pv-highlight'); if (h) h.style.outline = 'none'; lastHighlight = null; }, 3000);
      }
    };

    // ── onboarding wizard (parity: providers/new → ProviderOnboardingWizard) ──
    const openOnboarding = () => {
      const opts = catalog.map((p) => `<option value="${esc(p.id)}">${esc(p.name)}</option>`).join('');
      $('modal-card').innerHTML = `<h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main)">${esc(pw('onboardingWizard', 'Provider Onboarding Wizard'))}</h2>
        <p class="muted small">${esc(pw('addFirstProviderDesc', 'Connect an AI provider to start routing requests through OmniRoute.'))}</p>
        <div class="filter-row"><select id="pv-n-id" style="flex:2;min-width:180px">${opts}</select>
          <input id="pv-n-name" placeholder="${esc(pw('accountName', 'Name'))}" style="flex:1;min-width:120px"></div>
        <div class="filter-row" style="margin-top:8px"><input id="pv-n-key" placeholder="API key" style="flex:2;min-width:160px" autocomplete="off">
          <input id="pv-n-base" placeholder="Base URL (optional)" style="flex:2;min-width:160px"></div>
        <div class="filter-row" style="margin-top:8px"><input id="pv-n-models" placeholder="models (comma separated, optional)" style="flex:1;min-width:200px"></div>
        <div style="margin-top:10px;display:flex;gap:8px;justify-content:flex-end">
          <button class="mini" id="pv-n-cancel">${esc(cw('cancel', 'Cancel'))}</button>
          <button class="grad-btn" id="pv-n-go">+ ${esc(pw('addProvider', 'Add provider'))}</button></div>`;
      $('modal').style.display = 'flex';
      $('pv-n-cancel').addEventListener('click', () => { $('modal').style.display = 'none'; });
      $('pv-n-go').addEventListener('click', async () => {
        const pid = $('pv-n-id').value;
        const p = catalog.find((x) => x.id === pid) || { name: pid };
        const key = $('pv-n-key').value.trim();
        const body = {
          id: 'conn-' + Date.now().toString(36), provider: pid,
          name: $('pv-n-name').value.trim() || p.name,
          api_key: key || undefined,
          base_url: $('pv-n-base').value.trim() || undefined,
          models: $('pv-n-models').value.split(',').map((s) => s.trim()).filter(Boolean),
          enabled: true,
        };
        const r = await api('/v1/provider-connections', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }).catch(() => null);
        if (!r) { toast(pw('providerTestFailed', 'Provider test failed'), false); return; }
        $('modal').style.display = 'none';
        toast(pw('testSuccess', 'Connection saved'));
        await reloadConnections(); draw();
      });
    };
    // ── compatible endpoint modal (parity: AddCompatibleProviderModal) ──
    const openCompatModal = (kind) => {
      const prefix = kind === 'openai' ? 'openai-compatible-' : kind === 'cc' ? 'anthropic-compatible-cc-' : 'anthropic-compatible-';
      const title = kind === 'openai' ? pw('addOpenAICompatible', 'Add OpenAI-compatible') : kind === 'cc' ? pw('addCcCompatible', 'Add CC-compatible') : pw('addAnthropicCompatible', 'Add Anthropic-compatible');
      $('modal-card').innerHTML = `<h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main)">${esc(title)}</h2>
        <p class="muted small">ID: <code>${esc(prefix)}&lt;name&gt;</code></p>
        <div class="filter-row"><input id="pv-c-slug" placeholder="name (e.g. my-gateway)" style="flex:1;min-width:140px">
          <input id="pv-c-name" placeholder="${esc(pw('accountName', 'Name'))}" style="flex:1;min-width:140px"></div>
        <div class="filter-row" style="margin-top:8px"><input id="pv-c-base" placeholder="Base URL (required)" style="flex:1;min-width:200px">
          <input id="pv-c-key" placeholder="API key" style="flex:1;min-width:140px" autocomplete="off"></div>
        <div style="margin-top:10px;display:flex;gap:8px;justify-content:flex-end">
          <button class="mini" id="pv-c-cancel">${esc(cw('cancel', 'Cancel'))}</button>
          <button class="grad-btn" id="pv-c-go">+ ${esc(title)}</button></div>`;
      $('modal').style.display = 'flex';
      $('pv-c-cancel').addEventListener('click', () => { $('modal').style.display = 'none'; });
      $('pv-c-go').addEventListener('click', async () => {
        const slug = $('pv-c-slug').value.trim().toLowerCase().replace(/[^a-z0-9-]+/g, '-').replace(/^-+|-+$/g, '');
        const base = $('pv-c-base').value.trim();
        if (!slug || !base) { toast(pw('importErrorMissingName', 'name and base URL are required'), false); return; }
        const body = {
          id: 'conn-' + Date.now().toString(36), provider: prefix + slug,
          name: $('pv-c-name').value.trim() || slug,
          api_key: $('pv-c-key').value.trim() || undefined,
          base_url: base, enabled: true,
        };
        const r = await api('/v1/provider-connections', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }).catch(() => null);
        if (!r) { toast(pw('providerTestFailed', 'Provider test failed'), false); return; }
        $('modal').style.display = 'none';
        toast(pw('testSuccess', 'Connection saved'));
        await reloadConnections(); draw();
      });
    };
    // ── import modal (parity: ImportProvidersFromFileModal) ──
    const openImportModal = () => {
      $('modal-card').innerHTML = `<h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main)">${esc(pw('importFromFileTitle', 'Import providers from file'))}</h2>
        <p class="muted small">${esc(pw('importFromFileDescription', 'Paste a JSON array (or {"connections":[...]}) of provider connections, or pick a file.'))}</p>
        <div class="filter-row"><input type="file" id="pv-file" accept=".json,.csv" style="flex:1">
          <button class="mini" id="pv-tpl">${esc(pw('importFromFileDownloadTemplate', 'Template'))}</button></div>
        <textarea id="pv-json" rows="8" style="width:100%;margin-top:8px" placeholder='[{"provider":"openai","name":"main","apiKey":"sk-..."}]'></textarea>
        <div style="margin-top:10px;display:flex;gap:8px;justify-content:flex-end">
          <button class="mini" id="pv-json-cancel">${esc(cw('cancel', 'Cancel'))}</button>
          <button class="grad-btn" id="pv-json-go">${esc(pw('importFromFileImport', 'Import'))}</button></div>`;
      $('modal').style.display = 'flex';
      $('pv-json-cancel').addEventListener('click', () => { $('modal').style.display = 'none'; });
      $('pv-tpl').addEventListener('click', () => {
        const blob = new Blob([JSON.stringify([{ provider: 'openai', name: 'main', apiKey: 'sk-...', baseUrl: '', models: [] }], null, 2)], { type: 'application/json' });
        const a = document.createElement('a');
        a.href = URL.createObjectURL(blob); a.download = 'providers-template.json'; a.click();
      });
      $('pv-file').addEventListener('change', async () => {
        const f = $('pv-file').files[0];
        if (f) $('pv-json').value = await f.text();
      });
      $('pv-json-go').addEventListener('click', async () => {
        let parsed;
        try { parsed = JSON.parse($('pv-json').value); } catch { toast(pw('importErrorMalformedRow', 'invalid JSON'), false); return; }
        const r = await api('/v1/provider-connections/import', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(Array.isArray(parsed) ? { connections: parsed } : parsed) }).catch(() => null);
        if (!r) { toast(pw('providerTestFailed', 'Provider test failed'), false); return; }
        $('modal').style.display = 'none';
        toast(pw('importFromFileResult', '{imported} imported').replace('{imported}', r.imported) + ((r.errors || []).length ? `, ${r.errors.length} errors` : ''));
        await reloadConnections(); draw();
      });
    };

    const reloadConnections = async () => {
      const [c, conn] = await Promise.all([
        api('/v1/provider-catalog').catch(() => ({ providers: [], compatibleNodes: [] })),
        api('/v1/provider-connections').catch(() => ({ connections: [] })),
      ]);
      catalog = c.providers || [];
      compatibleNodes = c.compatibleNodes || [];
      connections = conn.connections || [];
    };
    const load = async () => {
      // restore filters from URL (parity: useProviderUrlFilters)
      try {
        const u = new URL(location.href);
        const q = u.searchParams.get('search'), m = u.searchParams.get('model'),
          md = u.searchParams.get('mode'), cc = u.searchParams.get('cat'), mk = u.searchParams.get('media');
        if (q) $('pv-q').value = q;
        if (m) $('pv-qm').value = m;
        if (['all', 'configured', 'compact'].includes(md)) mode = md;
        if (cc === 'free') { freeOnly = true; cat = null; }
        else if (cc) { freeOnly = false; cat = cc; }
        if (mk) mediaKind = mk;
      } catch {}
      await reloadConnections();
      if (mode === 'configured' && connections.length === 0) mode = 'all';
      draw();
    };

    $('pv-q').addEventListener('input', () => { syncUrl(); draw(); });
    $('pv-qm').addEventListener('input', () => { syncUrl(); draw(); });
    $('pv-mode').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => {
      mode = b.dataset.mode;
      if (mode === 'configured' && connections.length === 0) mode = 'all';
      syncUrl(); draw();
    }));
    $('pv-new').addEventListener('click', openOnboarding);
    $('pv-import-file').addEventListener('click', openImportModal);
    $('pv-test-all').addEventListener('click', () => batchTest('all'));
    await load();
  },
};

PAGES.combos = {
  title: 'Combos',
  body: () => {
    const pw = (k, fb) => T('sidebar.' + k) || fb;
    const cw = (k, fb) => T('common.' + k) || fb;
    const kw = (k, fb) => T('combos.' + k) || fb;
    return `
    <h1>${esc(pw('combos', 'Combos'))}</h1>
    <p class="muted small">${esc(pw('combosSubtitle', 'Create model combos with weighted routing and failover'))}</p>
    <div class="apikey-actions"><button class="grad-btn" id="cb-new">+ ${esc(kw('create', 'Create combo'))}</button></div>

    <div class="section-card">
      <button class="disclosure" id="cb-auto-toggle">
        <span class="material-symbols-outlined" style="color:var(--color-accent-light)">auto_awesome</span>
        <b>${esc(kw('autoCatalog', 'Automatic routing catalogue'))}</b>
        <span class="tag"><span id="cb-auto-count">—</span> ${esc(kw('templates', 'templates'))}</span>
        <span class="material-symbols-outlined chev">expand_more</span>
      </button>
      <div class="muted small" style="margin-top:6px">${esc(kw('autoHint', 'Built-in auto/* combos resolved dynamically from your connected providers. Use these IDs directly as the model field — no setup required.'))}</div>
      <div id="cb-auto-list" class="tpl-grid" style="display:none"></div>
    </div>

    <div class="preset-banner" id="cb-preset"></div>

    <div class="section-card" id="cb-guide">
      <div class="section-head">
        <span class="material-symbols-outlined" style="color:var(--warn)">lightbulb</span>
        <div><h3>${esc(kw('guideTitle', 'Combo getting-started guide'))}</h3>
          <div class="muted small">${esc(kw('guideHint', 'Create model combos to route AI traffic smartly'))}</div></div>
        <div class="row-actions">
          <a class="muted small" href="#" id="cb-hide">${esc(cw('hide', 'Hide'))}</a>
          <a class="muted small" href="#" id="cb-never">${esc(cw('dontShowAgain', "Don't show again"))}</a>
        </div>
      </div>
      <div class="steps">
        ${[['1', 'edit', kw('step1Title', 'Name your combo'), kw('step1Desc', 'Give the combo a unique name so routing rules can find it')],
           ['2', 'hub', kw('step2Title', 'Add models'), kw('step2Desc', 'Pick models and order them by failover priority')],
           ['3', 'share', kw('step3Title', 'Choose a strategy'), kw('step3Desc', 'How requests fan out across the models — 13 strategies available')],
           ['4', 'check_circle', kw('step4Title', 'Review and save'), kw('step4Desc', 'Review the configuration and activate the combo')]]
          .map(([n, icon, t, d], i) => `
          <div class="step-card">
            <span class="step-num">${n}</span>
            <span class="material-symbols-outlined" style="color:${['#f472b6', '#ef4444', '#22c55e', '#e54d5e'][i]}">${icon}</span>
            <b>${esc(t)}</b><span class="muted small">${esc(d)}</span>
          </div>`).join('<span class="material-symbols-outlined step-arrow">chevron_right</span>')}
      </div>
      <div class="callout">
        <b>${esc(kw('howToCall', 'How to call this combo'))}</b>
        <div class="muted small">${esc(kw('howToCallBody', 'Send the exact combo name as the model, e.g. model: "my-combo" (or combo/my-combo). auto and auto/* are a separate zero-config router that does not use your combos unless a combo is literally named auto.'))}</div>
      </div>
      <div style="margin-top:14px;display:flex;align-items:center;gap:10px">
        <button class="grad-btn" id="cb-first">+ ${esc(kw('createFirst', 'Create your first combo'))}</button>
        <span class="muted small">${esc(kw('orUseTop', 'or use 「+ Create combo」 above'))}</span>
      </div>
    </div>

    <div class="section-title">
      <div class="segmented" id="cb-filter">
        <button data-c="all" class="active">${esc(cw('all', 'All'))} <b id="cb-n-all">0</b></button>
        <button data-c="smart"><span class="material-symbols-outlined">auto_awesome</span>${esc(kw('smartRouting', 'Smart routing'))} <b id="cb-n-smart">0</b></button>
        <button data-c="deterministic"><span class="material-symbols-outlined">drag_handle</span>${esc(kw('deterministic', 'Deterministic'))} <b id="cb-n-det">0</b></button>
      </div>
    </div>
    <div id="cb-rows"></div>`;
  },
  after: async () => {
    const cw = (k, fb) => T('common.' + k) || fb;
    const kw = (k, fb) => T('combos.' + k) || fb;
    let combos = [];
    let filter = 'all';
    const seen = () => localStorage.getItem('omniroute_combo_guide') === 'hidden';

    const draw = () => {
      const list = combos.filter((c) => filter === 'all' || c.category === filter);
      $('cb-n-all').textContent = combos.length;
      $('cb-n-smart').textContent = combos.filter((c) => c.category === 'smart').length;
      $('cb-n-det').textContent = combos.filter((c) => c.category === 'deterministic').length;
      $('cb-rows').innerHTML = list.length ? list.map((c) => `
        <div class="combo-row" data-id="${esc(c.id)}">
          <span class="material-symbols-outlined drag">drag_indicator</span>
          <span class="material-symbols-outlined" style="color:${iconAccent(c.name)}">${c.category === 'smart' ? 'auto_awesome' : 'layers'}</span>
          <div class="combo-name"><b>${esc(c.name)}</b>
            <div class="muted small">${esc(c.strategy)} · ${esc((c.providers || []).join(' → ') || (c.models || []).join(', '))}</div>
            <div>${(c.tags || []).map((t) => `<span class="tag">${esc(t)}</span>`).join('')}${c.source === 'config' ? `<span class="tag">${esc(cw('readOnly', 'config (read-only)'))}</span>` : ''}</div>
          </div>
          <div class="combo-actions">
            <label class="switch"><input type="checkbox" data-act="enable" ${c.enabled ? 'checked' : ''} ${c.source === 'config' ? 'disabled' : ''}><span></span></label>
            <select data-act="default">${(c.models || []).map((m) => `<option ${m === c.default_model ? 'selected' : ''}>${esc(m)}</option>`).join('') || '<option>—</option>'}</select>
            <button class="icon-btn" data-act="run" title="${esc(cw('testConnection', 'Test'))}"><span class="material-symbols-outlined">play_arrow</span></button>
            <button class="icon-btn" data-act="copy" title="${esc(cw('copy', 'Copy'))}"><span class="material-symbols-outlined">content_copy</span></button>
            <button class="icon-btn" data-act="edit" title="${esc(cw('edit', 'Edit'))}"><span class="material-symbols-outlined">edit</span></button>
            <button class="icon-btn danger" data-act="del" title="${esc(cw('delete', 'Delete'))}" ${c.source === 'config' ? 'disabled' : ''}><span class="material-symbols-outlined">delete</span></button>
          </div>
        </div>`).join('') : `<div class="na-note">${esc(kw('empty', 'No combos yet — create one above'))}</div>`;

      document.querySelectorAll('.combo-row').forEach((row) => {
        const id = row.dataset.id;
        const combo = combos.find((c) => c.id === id);
        row.querySelectorAll('[data-act]').forEach((el) => {
          const act = el.dataset.act;
          if (act === 'default') { el.addEventListener('change', () => toast(kw('defaultSaved', 'default model saved (applies to the next request)'))); return; }
          if (act === 'run') el.addEventListener('click', async () => {
            const model = (combo.models || [])[0] || combo.name;
            const r = await fetch('/v1/chat/completions', {
              method: 'POST',
              headers: { 'content-type': 'application/json', authorization: 'Bearer ' + (localStorage.getItem('omniroute_session') || '') },
              body: JSON.stringify({ model, messages: [{ role: 'user', content: 'ping' }], max_tokens: 4 }),
            });
            toast(`${model}: HTTP ${r.status}`, r.ok);
          });
          if (act === 'copy') el.addEventListener('click', async () => { await navigator.clipboard.writeText(combo.name); toast(cw('copied', 'copied')); });
          if (act === 'edit') el.addEventListener('click', () => openForm(combo));
          if (act === 'del' && !el.disabled) el.addEventListener('click', async () => {
            if (!confirm(kw('deleteConfirm', 'Delete this combo?'))) return;
            await api('/v1/combos/managed/' + encodeURIComponent(id), { method: 'DELETE' });
            load();
          });
          if (act === 'enable' && !el.disabled) el.addEventListener('change', async () => {
            await api('/v1/combos/managed/' + encodeURIComponent(id), { method: 'PATCH', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ enabled: el.checked }) });
            toast(el.checked ? cw('enabled', 'enabled') : cw('disabled', 'disabled'));
          });
        });
      });
      if (seen()) $('cb-guide').style.display = 'none';
    };

    const openForm = (existing) => {
      const c = existing || { name: '', strategy: 'priority', providers: [], models: [] };
      $('modal-card').innerHTML = `
        <h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main)">${esc(existing ? kw('edit', 'Edit combo') : kw('create', 'Create combo'))}</h2>
        <div class="combo" style="background:none;border:0;padding:0;margin-top:10px">
          <label>${esc(kw('step1Title', 'Name your combo'))}</label><input type="text" id="cb-name" value="${esc(c.name)}" style="width:100%">
          <label>${esc(kw('step3Title', 'Choose a strategy'))}</label>
          <select id="cb-strategy" style="width:100%">${['priority', 'round-robin', 'fill-first', 'weighted', 'random', 'least-used', 'p2c', 'cost-optimized', 'lkgp', 'auto'].map((x) => `<option ${x === c.strategy ? 'selected' : ''}>${x}</option>`).join('')}</select>
          <label>${esc(kw('step2Title', 'Add models'))} (provider or provider/model, comma separated)</label>
          <input type="text" id="cb-providers" value="${esc((c.providers || []).join(', '))}" style="width:100%" placeholder="anthropic/claude-sonnet-4-5, openai/gpt-4o">
          <label>models (comma separated)</label><input type="text" id="cb-models" value="${esc((c.models || []).join(', '))}" style="width:100%">
          <div style="margin-top:12px"><button class="save" id="cb-save2">${esc(cw('save', 'Save'))}</button></div>
        </div>`;
      $('modal').style.display = 'flex';
      $('cb-save2').addEventListener('click', async () => {
        const body = {
          id: existing ? existing.id : '',
          name: $('cb-name').value.trim(),
          strategy: $('cb-strategy').value,
          providers: $('cb-providers').value.split(',').map((s) => s.trim()).filter(Boolean),
          models: $('cb-models').value.split(',').map((s) => s.trim()).filter(Boolean),
          enabled: true,
        };
        try {
          await api('/v1/combos/managed', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
          $('modal').style.display = 'none';
          toast(cw('saved', 'saved'));
          load();
        } catch (e) { toast('save failed: ' + e.message, false); }
      });
    };

    const load = async () => {
      const [c, presets] = await Promise.all([
        api('/v1/combos/managed').catch(() => ({ combos: [] })),
        api('/v1/combo-presets').catch(() => null),
      ]);
      combos = c.combos || [];
      if (presets) {
        $('cb-auto-count').textContent = presets.total;
        $('cb-auto-list').innerHTML = (presets.templates || []).map((t, i) => `
          <div class="tpl-card">
            <div class="tpl-head">
              <code class="tpl-id">${esc(t.id)}</code>
              <span class="tpl-strategy">${esc((t.strategy || '').toUpperCase())}</span>
            </div>
            <b class="tpl-title">${esc(t.title || t.id)}</b>
            <div class="tpl-tags">${(t.tags || []).map((tag, j) => `<span class="tpl-tag ${j === 0 ? 'lead' : ''}">${esc(tag)}</span>`).join('')}</div>
            ${t.prompt ? `<div class="tpl-prompt">${esc(t.prompt)}</div>` : ''}
            <button class="icon-btn tpl-copy" data-copy="${esc(t.id)}" title="${esc(cw('copy', 'Copy'))}">
              <span class="material-symbols-outlined">content_copy</span></button>
          </div>`).join('');
        $('cb-auto-list').querySelectorAll('[data-copy]').forEach((b) => b.addEventListener('click', async () => {
          await navigator.clipboard.writeText(b.dataset.copy);
          toast(cw('copied', 'copied') + ': ' + b.dataset.copy);
        }));
        const p = (presets.presets || [])[0];
        if (p) {
          $('cb-preset').innerHTML = `
            <span class="material-symbols-outlined" style="color:#f472b6">bolt</span>
            <div><b>${esc(p.name)}</b>
              <div class="muted small">${esc(p.description)}</div>
              <div class="muted small">${esc(kw('primary', 'primary'))}: <code>${esc(p.primary)}</code> · ${esc(kw('fallbacks', 'fallbacks'))}: ${esc((p.fallbacks || []).join(', '))} ${p.ready ? '' : `· <span class="s-err">${esc(kw('notConnected', 'connect a Kimi account first'))}</span>`}</div>
            </div>
            <button class="grad-btn" id="cb-add-preset">+ ${esc(kw('addPreset', 'Add preset'))}</button>`;
          $('cb-add-preset').addEventListener('click', async () => {
            await api('/v1/combos/managed', {
              method: 'POST', headers: { 'content-type': 'application/json' },
              body: JSON.stringify({ name: 'kimi-coding', strategy: 'priority', providers: [p.primary, ...(p.fallbacks || [])], models: ['kimi-coding'] }),
            });
            toast(kw('presetAdded', 'preset added'));
            load();
          });
        }
      }
      draw();
    };

    $('cb-auto-toggle').addEventListener('click', () => {
      const l = $('cb-auto-list');
      const open = l.style.display !== 'none';
      l.style.display = open ? 'none' : 'flex';
      $('cb-auto-toggle').querySelector('.chev').textContent = open ? 'expand_more' : 'expand_less';
    });
    $('cb-hide').addEventListener('click', (e) => { e.preventDefault(); $('cb-guide').style.display = 'none'; });
    $('cb-never').addEventListener('click', (e) => { e.preventDefault(); localStorage.setItem('omniroute_combo_guide', 'hidden'); $('cb-guide').style.display = 'none'; });
    $('cb-filter').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => {
      $('cb-filter').querySelectorAll('button').forEach((x) => x.classList.remove('active'));
      b.classList.add('active');
      filter = b.dataset.c;
      draw();
    }));
    $('cb-new').onclick = () => openForm(null);
    $('cb-first').onclick = () => openForm(null);
    await load();
  },
};

PAGES.quota = {
  title: 'Provider Quota',
  body: () => {
    const pw = (k, fb) => T('sidebar.' + k) || fb;
    const cw = (k, fb) => T('common.' + k) || fb;
    const qw = (k, fb) => T('quota.' + k) || fb;
    return `
    <div class="section-title">
      <h2 style="margin:0">${esc(qw('title', 'Provider limits'))}</h2>
      <span class="muted small" id="pq-count">—</span>
      <div class="row-actions">
        <div class="segmented"><button class="active"><span class="material-symbols-outlined">grid_view</span>Full</button></div>
        <button class="mini" id="pq-refresh-all"><span class="material-symbols-outlined" style="font-size:15px;vertical-align:-3px">refresh</span> ${esc(qw('refreshAll', 'Refresh all'))}</button>
      </div>
    </div>

    <div class="kpi-row" id="pq-summary"></div>

    <div class="filter-card">
      <div class="chip-row">
        <span class="muted small" style="align-self:center">${esc(qw('type', 'Type'))}:</span>
        <button class="chip active" data-kind="type" data-v="all">${esc(cw('all', 'All'))} <b id="pq-t-all">0</b></button>
        <button class="chip" data-kind="type" data-v="apikey">API ${esc(qw('key', 'key'))} <b id="pq-t-key">0</b></button>
        <button class="chip" data-kind="type" data-v="oauth">OAuth <b id="pq-t-oauth">0</b></button>
      </div>
      <div class="chip-row">
        <span class="muted small" style="align-self:center">${esc(qw('tier', 'Tier'))}:</span>
        <button class="chip active" data-kind="tier" data-v="all">${esc(cw('all', 'All'))} <b id="pq-tier-all">0</b></button>
        <button class="chip" data-kind="tier" data-v="unknown">${esc(qw('unknown', 'Unknown'))} <b id="pq-tier-unknown">0</b></button>
        <button class="chip" data-kind="tier" data-v="free">${esc(cw('free', 'Free'))} <b id="pq-tier-free">0</b></button>
        <button class="chip" data-kind="tier" data-v="paid">${esc(qw('paid', 'Paid'))} <b id="pq-tier-paid">0</b></button>
      </div>
      <div class="filter-row" style="margin-top:10px">
        <span class="muted small">PROVIDER</span>
        <select id="pq-provider"><option value="all">${esc(qw('allProviders', 'All providers'))}</option></select>
      </div>
    </div>

    <div id="pq-accounts"></div>`;
  },
  after: async () => {
    const cw = (k, fb) => T('common.' + k) || fb;
    const qw = (k, fb) => T('quota.' + k) || fb;
    let accounts = [];
    let summary = {};
    let kind = 'all';
    let tier = 'all';
    let provider = 'all';

    const draw = () => {
      $('pq-count').textContent = `${summary.total || 0} ${qw('accounts', 'accounts')}`;
      $('pq-summary').innerHTML = [
        [qw('total', 'Total'), summary.total || 0, ''],
        [qw('critical', 'Critical'), summary.critical || 0, 'crit'],
        [qw('warning', 'Warning'), summary.warning || 0, 'warn'],
        [qw('healthy', 'Healthy'), summary.healthy || 0, 'ok'],
      ].map(([l, v, cls]) => `<div class="kpi ${cls}"><div class="kpi-label">${esc(l)}</div><div class="kpi-value ${cls}">${v}</div></div>`).join('');
      $('pq-t-all').textContent = accounts.length;
      $('pq-t-key').textContent = accounts.filter((a) => a.authKind !== 'oauth').length;
      $('pq-t-oauth').textContent = accounts.filter((a) => a.authKind === 'oauth').length;
      $('pq-tier-all').textContent = accounts.length;
      $('pq-tier-unknown').textContent = accounts.filter((a) => (a.tier || 'unknown') === 'unknown').length;
      $('pq-tier-free').textContent = accounts.filter((a) => a.tier === 'free').length;
      $('pq-tier-paid').textContent = accounts.filter((a) => a.tier === 'paid').length;
      $('pq-provider').innerHTML = `<option value="all">${esc(qw('allProviders', 'All providers'))}</option>` +
        accounts.map((a) => `<option value="${esc(a.provider)}" ${a.provider === provider ? 'selected' : ''}>${esc(a.provider)}</option>`).join('');

      const list = accounts.filter((a) =>
        (kind === 'all' || (kind === 'oauth' ? a.authKind === 'oauth' : a.authKind !== 'oauth')) &&
        (tier === 'all' || (a.tier || 'unknown') === tier) &&
        (provider === 'all' || a.provider === provider));

      $('pq-accounts').innerHTML = list.length ? list.map((a) => `
        <div class="quota-card" data-p="${esc(a.provider)}">
          <div class="quota-head">
            <span class="material-symbols-outlined" style="color:${iconAccent(a.provider)}">dns</span>
            <div><b>${esc(a.provider)}</b>
              <div class="muted small">${a.active ? '1 active' : '0 active'} / 1 ${qw('account', 'account')}</div></div>
            <span class="tag">${esc(a.provider)}</span>
            <span class="status-pill ${a.severity}"><i></i>${esc(a.severity)}</span>
            <label class="switch"><input type="checkbox" data-act="toggle" ${a.active ? 'checked' : ''}><span></span></label>
          </div>
          <div class="quota-account">
            <span class="dot ${a.severity === 'healthy' ? 'ok' : ''}"></span>
            <b>${esc(a.provider)}</b><span class="tag">${esc(a.tier || 'unknown')}</span>
            <span class="muted small" style="margin-left:auto">${a.windowHits} / ${a.cutoff ?? '—'} rpm · ${a.concurrent} in-flight</span>
          </div>
          ${a.note ? `<div class="quota-note">${esc(a.note)}</div>` : ''}
          ${a.balance != null ? `<div class="quota-balance"><span class="material-symbols-outlined">payments</span> ${esc(a.currency || 'USD')} <b>${a.balance}</b></div>` : ''}
          <div class="quota-foot">
            <span class="muted small">${qw('updatedAt', 'updated at')}: ${a.updatedAtMs ? new Date(a.updatedAtMs).toLocaleTimeString() : '—'}</span>
            <div class="row-actions">
              <button class="mini" data-act="cutoff"><span class="material-symbols-outlined" style="font-size:14px;vertical-align:-3px">tune</span> ${esc(qw('editCutoff', 'Edit cutoff'))}</button>
              <button class="mini" data-act="cost"><span class="material-symbols-outlined" style="font-size:14px;vertical-align:-3px">bar_chart</span> ${esc(qw('usdCost', 'USD cost'))}</button>
              <button class="mini" data-act="refresh"><span class="material-symbols-outlined" style="font-size:14px;vertical-align:-3px">refresh</span> ${esc(qw('refreshNow', 'Refresh now'))}</button>
            </div>
          </div>
        </div>`).join('') : `<div class="na-note">${esc(qw('empty', 'No provider accounts yet — add a provider connection first'))}</div>`;

      document.querySelectorAll('.quota-card').forEach((card) => {
        const p = card.dataset.p;
        card.querySelectorAll('[data-act]').forEach((el) => {
          const act = el.dataset.act;
          if (act === 'cutoff') el.addEventListener('click', async () => {
            const v = prompt(`${qw('editCutoff', 'Edit cutoff')} (rpm ceiling, ${p}):`, accounts.find((a) => a.provider === p)?.cutoff ?? '');
            if (v === null) return;
            await api('/v1/provider-quotas/' + encodeURIComponent(p), { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ cutoff: v === '' ? null : Number(v) }) });
            toast(cw('saved', 'saved'));
            load();
          });
          if (act === 'cost') el.addEventListener('click', async () => {
            const v = prompt(`${qw('usdCost', 'USD cost / balance')} (${p}):`, accounts.find((a) => a.provider === p)?.balance ?? '');
            if (v === null) return;
            const cur = prompt(qw('currency', 'currency code:'), 'USD') || 'USD';
            await api('/v1/provider-quotas/' + encodeURIComponent(p), { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ balance: v === '' ? null : Number(v), currency: cur }) });
            toast(cw('saved', 'saved'));
            load();
          });
          if (act === 'refresh') el.addEventListener('click', () => { toast(qw('refreshed', 'quota refreshed from live circuit state')); load(); });
          if (act === 'toggle') el.addEventListener('change', async () => {
            await api('/v1/provider-quotas/' + encodeURIComponent(p), { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ tier: el.checked ? 'paid' : 'free' }) });
            toast(el.checked ? cw('enabled', 'enabled') : cw('disabled', 'disabled'));
            load();
          });
        });
      });
    };

    const load = async () => {
      const v = await api('/v1/provider-quotas').catch(() => null);
      if (!v) { $('pq-accounts').innerHTML = '<div class="na-note">quota data unavailable</div>'; return; }
      accounts = v.accounts || [];
      summary = v.summary || {};
      draw();
    };
    document.querySelectorAll('[data-kind]').forEach((b) => b.addEventListener('click', () => {
      const k = b.dataset.kind;
      document.querySelectorAll(`[data-kind="${k}"]`).forEach((x) => x.classList.remove('active'));
      b.classList.add('active');
      if (k === 'type') kind = b.dataset.v; else tier = b.dataset.v;
      draw();
    }));
    $('pq-provider').addEventListener('change', () => { provider = $('pq-provider').value; draw(); });
    $('pq-refresh-all').addEventListener('click', () => { toast(qw('refreshed', 'quota refreshed from live circuit state')); load(); });
    await load();
  },
};

PAGES.usage = {
  title: 'Usage',
  body: () => {
    const cw = (k, fb) => T('common.' + k) || fb;
    const uw = (k, fb) => T('analytics.' + k) || fb;
    return `
    <div class="tabbar" id="usage-tabs">
      <button class="active" data-t="overview"><span class="material-symbols-outlined">analytics</span>${esc(uw('overview', 'Overview'))}</button>
      <button data-t="evals"><span class="material-symbols-outlined">science</span>${esc(uw('evals', 'Evals'))}</button>
      <button data-t="search"><span class="material-symbols-outlined">search</span>${esc(uw('search', 'Search'))}</button>
      <button data-t="utilization"><span class="material-symbols-outlined">speed</span>${esc(uw('utilization', 'Utilization'))}</button>
      <button data-t="combo-health"><span class="material-symbols-outlined">monitor_heart</span>${esc(uw('comboHealth', 'Combo health'))}</button>
      <button data-t="cache"><span class="material-symbols-outlined">database</span>Cache Health</button>
      <button data-t="tracing"><span class="material-symbols-outlined">route</span>${esc(uw('tracing', 'Route tracing'))}</button>
    </div>

    <div class="section-title">
      <h2 style="margin:0">${esc(uw('usageAnalytics', 'Usage analytics'))}</h2>
      <div class="row-actions">
        <select id="ua-key"><option value="all">${esc(uw('allKeys', 'All keys'))}</option></select>
        <div class="segmented" id="ua-range">
          <button data-r="1">1${esc(uw('dayShort', 'd'))}</button>
          <button data-r="7">7${esc(uw('dayShort', 'd'))}</button>
          <button data-r="30" class="active">30${esc(uw('dayShort', 'd'))}</button>
          <button data-r="90">90${esc(uw('dayShort', 'd'))}</button>
          <button data-r="all">${esc(cw('all', 'All'))}</button>
        </div>
      </div>
    </div>

    <div class="kpi-row" id="ua-kpi"></div>
    <div class="panel" id="ua-metrics"></div>
    <div class="panels" style="grid-template-columns:2fr 1fr">
      <div class="panel">
        <h3>${esc(uw('overview', 'Overview'))}</h3>
        <div class="panel-sub" id="ua-overview-sub"></div>
        <div id="ua-heatmap" class="heatmap"></div>
        <div class="legend" style="margin-top:10px">
          <span>${esc(uw('less', 'less'))}</span>
          <span class="heat-1"></span><span class="heat-2"></span><span class="heat-3"></span><span class="heat-4"></span>
          <span>${esc(uw('more', 'more'))}</span>
        </div>
      </div>
      <div>
        <div class="panel"><h3>${esc(uw('busiestDay', 'Busiest day'))}</h3>
          <div class="n" id="ua-busiest-day" style="font-size:20px;font-weight:600">—</div>
          <div class="muted small" id="ua-busiest-val"></div></div>
        <div class="panel"><h3>${esc(uw('weekly', 'Weekly'))}</h3>
          <div class="bars" id="ua-weekly"></div></div>
      </div>
    </div>`;
  },
  after: async () => {
    const cw = (k, fb) => T('common.' + k) || fb;
    const uw = (k, fb) => T('analytics.' + k) || fb;
    const A = await api('/v1/usage/analytics').catch(() => null);
    if (!A) { $('ua-kpi').innerHTML = '<div class="na-note">analytics unavailable</div>'; return; }
    const s = A.summary || {};
    const fmt = (n) => {
      const v = Number(n || 0);
      return v >= 1e9 ? (v / 1e9).toFixed(1) + 'B' : v >= 1e6 ? (v / 1e6).toFixed(1) + 'M' : v >= 1e3 ? (v / 1e3).toFixed(1) + 'K' : String(v);
    };
    $('ua-kpi').innerHTML = [
      [fmt(s.totalTokens), uw('totalTokens', 'Total tokens'), `${s.totalRequests || 0} ${cw('requests', 'requests')}`, ''],
      [fmt(s.promptTokens), uw('inputTokens', 'Input tokens'), '', 'pink'],
      [fmt(s.completionTokens), uw('outputTokens', 'Output tokens'), '', 'pink'],
      [`$${Number(s.totalCost || 0).toFixed(6)}`, uw('estimatedCost', 'Estimated cost'), '', 'warn'],
    ].map(([v, l, sub, cls]) => `<div class="kpi ${cls}"><div class="kpi-label">${esc(l)}</div>
      <div class="kpi-value ${cls}">${v}</div><div class="muted small">${esc(sub)}</div></div>`).join('');

    const rows = [
      [uw('infra', 'Infrastructure'), [
        [uw('accounts', 'Accounts'), s.uniqueAccounts], [uw('providers', 'Providers'), s.uniqueAccounts],
        [uw('apiKeys', 'API keys'), s.uniqueApiKeys], [uw('models', 'Models'), s.uniqueModels],
      ]],
      [uw('performance', 'Performance'), [
        [uw('avgTokensPerRequest', 'Avg tokens / request'), fmt(s.totalRequests ? Math.round((s.totalTokens || 0) / s.totalRequests) : 0)],
        [uw('costPerRequest', 'Cost / request'), `$${(s.totalRequests ? (s.totalCost || 0) / s.totalRequests : 0).toFixed(6)}`],
        [uw('inOutRatio', 'Input / output ratio'), `${(s.completionTokens ? ((s.promptTokens || 0) / s.completionTokens).toFixed(1) : '0')}×`],
        [uw('successRate', 'Success rate'), `${s.successRatePct || 0}%`],
      ]],
      [uw('highlights', 'Highlights'), [
        [uw('topModel', 'Top model'), (A.byModel || [])[0] ? A.byModel[0].model : '—'],
        [uw('topProvider', 'Top provider'), (A.byProvider || [])[0] ? A.byProvider[0].provider : '—'],
        [uw('busiestDay', 'Busiest day'), Object.entries(A.activityMap || {}).sort((a, b) => b[1] - a[1])[0]?.[0] || '—'],
        [uw('fallbackRate', 'Fallback rate'), `${s.totalRequests ? (((s.fallbackCount || 0) / s.totalRequests) * 100).toFixed(1) : '0.0'}%`],
      ]],
    ];
    $('ua-metrics').innerHTML = rows.map(([title, items]) => `
      <h4 class="metric-group">${esc(title)}</h4>
      <div class="metric-grid">${items.map(([k, v]) => `<div><span class="muted small">${esc(k)}</span><b>${esc(String(v))}</b></div>`).join('')}</div>`).join('');

    const act = A.activityMap || {};
    const entries = Object.entries(act).sort((a, b) => a[0].localeCompare(b[0]));
    const max = entries.reduce((m, [, v]) => Math.max(m, v), 0) || 1;
    $('ua-heatmap').innerHTML = entries.map(([d, v]) => {
      const lvl = Math.min(4, Math.ceil((v / max) * 4));
      return `<span class="heat-cell heat-${lvl || 1}" title="${esc(d)}: ${fmt(v)} ${cw('tokensShort', 'tokens')}"></span>`;
    }).join('') || '<span class="muted small">no activity yet</span>';
    $('ua-overview-sub').textContent = `${entries.length} ${uw('activeDays', 'active days')} · ${fmt(s.totalTokens || 0)} ${cw('tokensShort', 'tokens')}`;
    const busiest = entries.slice().sort((a, b) => b[1] - a[1])[0];
    if (busiest) {
      $('ua-busiest-day').textContent = new Date(busiest[0] + 'T00:00:00Z').toLocaleDateString(undefined, { weekday: 'long' });
      $('ua-busiest-val').textContent = `${busiest[0]} · ${fmt(busiest[1])} ${cw('tokensShort', 'tokens')}`;
    }
    const wt = A.weeklyTokens || [];
    const wmax = Math.max(1, ...wt);
    const days = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
    $('ua-weekly').innerHTML = wt.map((v, i) => `
      <div class="bar-col" title="${days[i]}: ${fmt(v)}">
        <div class="bar" style="height:${Math.max(3, Math.round((v / wmax) * 70))}px"></div>
        <span class="muted small">${days[i][0]}</span>
      </div>`).join('');

    $('ua-range').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => {
      $('ua-range').querySelectorAll('button').forEach((x) => x.classList.remove('active'));
      b.classList.add('active');
      toast(uw('rangeNote', 'the ring keeps the last 500 requests — range selection is display-only for now'));
    }));
  },
};

PAGES.logs = {
  title: 'Logs',
  body: () => `
    <h1>Request log</h1>
    <p class="muted small">in-memory ring (last 500) — click a row for the full entry (traffic-inspector detail)</p>
    <table><thead><tr><th>time</th><th>model</th><th>provider</th><th>status</th><th>ms</th><th>tokens saved</th></tr></thead><tbody id="log-rows"></tbody></table>`,
  after: async () => {
    const logs = await api('/v1/logs?limit=200');
    const rows = logs.logs || [];
    $('log-rows').innerHTML = rows.length ? rows.map((l, i) =>
      `<tr data-row="${i}" style="cursor:pointer"><td>${new Date(l.ts_ms).toLocaleTimeString()}</td><td>${esc(l.model)}</td><td>${esc(l.provider || '-')}</td><td>${statusBadge(l.status)}</td><td>${l.latency_ms}</td><td>${l.tokens_saved || 0}</td></tr>`).join('')
      : '<tr><td colspan="6" class="muted small">no requests recorded yet</td></tr>';
    $('log-rows').querySelectorAll('tr[data-row]').forEach((tr) => tr.addEventListener('click', () => {
      const l = rows[Number(tr.dataset.row)];
      if (!l) return;
      const kv = (k, val) => `<div class="endpoint-row"><b style="min-width:150px">${esc(k)}</b><span>${esc(val)}</span></div>`;
      $('modal-card').innerHTML = `
        <h2 style="text-transform:none;letter-spacing:0;font-size:15px;color:var(--color-text-main)">Request detail</h2>
        ${kv('time', new Date(l.ts_ms).toLocaleString())}
        ${kv('model', l.model || '-')}
        ${kv('provider', l.provider || '-')}
        ${kv('status', l.status)}
        ${kv('latency', (l.latency_ms || 0) + ' ms')}
        ${kv('stream', l.stream ? 'yes' : 'no')}
        ${kv('compressed', l.compressed ? 'yes' : 'no')}
        ${kv('tokens saved', l.tokens_saved || 0)}
        ${kv('prompt / completion tokens', (l.prompt_tokens || 0) + ' / ' + (l.completion_tokens || 0))}
        <div style="margin-top:10px;text-align:right"><button class="mini" id="log-detail-close">Close</button></div>`;
      $('modal').style.display = 'flex';
      $('log-detail-close').addEventListener('click', () => { $('modal').style.display = 'none'; });
    }));
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
  title: 'Compression Settings',
  body: () => {
    const pw = (k, fb) => T('sidebar.' + k) || fb;
    const cw = (k, fb) => T('common.' + k) || fb;
    return `
    <div class="section-card">
      <div class="section-head">
        <span class="plogo" style="background:rgba(99,102,241,.16);color:var(--color-accent-light)"><span class="material-symbols-outlined">compress</span></span>
        <div>
          <h3>${esc(pw('contextSettings', 'Prompt compression'))}</h3>
          <div class="muted small">${esc(pw('contextSettingsSubtitle', 'Compress prompts before they reach the provider to cut token usage'))}</div>
          <a class="muted small" href="https://github.com/diegosouzapw/OmniRoute" target="_blank">${esc(cw('fullDocs', 'Full compression guide'))} ↗</a>
        </div>
        <label class="switch" style="margin-left:auto"><input type="checkbox" id="c-enabled"><span></span></label>
      </div>
      <div class="info-strip" id="c-pipeline">—</div>
      <div class="info-strip" id="c-autobudget">—</div>
    </div>
    <div class="section-card" id="c-engines"></div>
    <div class="section-card">
      <h3 style="margin:0 0 4px">${esc(pw('contextGroup', 'Engines'))}</h3>
      <div class="muted small" style="margin-bottom:10px">${esc(cw('manualConfig', 'Runtime values'))}</div>
      <div class="metric-grid" id="c-values"></div>
      <div style="margin-top:12px"><button class="save" id="c-save">${esc(cw('save', 'Save'))}</button>
        <span id="c-msg" class="small" style="margin-left:10px"></span></div>
    </div>`;
  },
  after: async () => {
    const cw = (k, fb) => T('common.' + k) || fb;
    const c = await api('/v1/compression');
    const ENGINES = [
      ['session-dedup', 'SESSION-DEDUP', 'Cross-turn block deduplication', 'safe', false],
      ['ccr', 'CCR', 'Content-addressed retrieval markers', 'safe', false],
      ['lite', 'LITE', 'Whitespace and formatting cleanup', 'safe', false],
      ['rtk', 'RTK', 'Command-output filtering', null, true],
      ['codex-responses', 'CODEX-RESPONSES', 'Conservative compaction of supported Responses tool output', null, false],
      ['headroom', 'HEADROOM', 'Tabular JSON compaction', 'safe', false],
      ['caveman', 'CAVEMAN', 'Rule-engine prompt compression', 'safe', false],
      ['aggressive', 'AGGRESSIVE', 'Summary + aging of old turns', null, false],
      ['ultra', 'ULTRA', 'Heuristic score pruning', null, false],
    ];
    const active = c.enabled;
    $('c-enabled').checked = active;
    $('c-pipeline').innerHTML = `<b>${cw('activePipeline', 'Effective pipeline')}</b>: ${cw('mode', 'mode')}: <code>${esc(c.default_mode)}</code>`;
    $('c-autobudget').innerHTML = `<b>${cw('adaptiveBudget', 'Adaptive context budget')}</b>: ${c.auto_trigger_tokens > 0 ? `${c.auto_trigger_tokens} tokens → ${esc(c.auto_trigger_mode)}` : esc(cw('disabled', 'off'))}`;
    $('c-engines').innerHTML = `<h3 style="margin:0 0 8px">${esc(cw('engines', 'Engines'))}</h3>` + ENGINES.map(([id, label, desc, safety, hasIntensity]) => `
      <div class="engine-row">
        <div class="engine-main">
          <div class="engine-title">${esc(label)} <span class="muted small">${esc(id.toUpperCase())}</span></div>
          <div class="muted small">${esc(desc)}</div>
          <div class="engine-badges">
            ${safety ? `<span class="tag">${esc(cw('safeDefault', 'safe default'))}</span>` : ''}
            <a class="muted small" href="#" data-detail="${id}">${esc(cw('details', 'details'))}</a>
          </div>
        </div>
        <div class="engine-controls">
          ${hasIntensity ? `<select data-intensity="${id}">${['minimal', 'standard', 'aggressive'].map((x) => `<option ${x === (c.rtk_intensity || 'standard') ? 'selected' : ''}>${x}</option>`).join('')}</select>` : ''}
          <label class="switch"><input type="checkbox" data-engine="${id}" ${active && ['lite', 'caveman'].includes(id) ? 'checked' : ''}><span></span></label>
        </div>
      </div>`).join('');
    $('c-engines').querySelectorAll('[data-detail]').forEach((a) => a.addEventListener('click', (e) => {
      e.preventDefault();
      toast(`${a.dataset.detail}: ${cw('detailsInDocs', 'see the compression guide for the full rule set')}`);
    }));
    $('c-values').innerHTML = Object.entries({
      default_mode: c.default_mode,
      auto_trigger_tokens: c.auto_trigger_tokens,
      auto_trigger_mode: c.auto_trigger_mode,
      caveman_intensity: c.caveman_intensity,
      preserve_system_prompt: c.preserve_system_prompt,
      compress_roles: (c.compress_roles || []).join(','),
      min_message_length: c.min_message_length,
      ultra_compression_rate: c.ultra_compression_rate,
      ultra_min_score: c.ultra_min_score,
      aggressive_max_tokens_per_message: c.aggressive_max_tokens_per_message,
      aggressive_min_savings: c.aggressive_min_savings,
      rtk_max_lines: c.rtk_max_lines,
    }).map(([k, v]) => `<div><span class="muted small">${esc(k)}</span><b>${esc(String(v))}</b></div>`).join('');
    $('c-enabled').addEventListener('change', async () => {
      await api('/v1/compression', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ enabled: $('c-enabled').checked }) });
      toast($('c-enabled').checked ? cw('enabled', 'enabled') : cw('disabled', 'disabled'));
    });
    $('c-save').addEventListener('click', async () => {
      const body = {
        enabled: $('c-enabled').checked,
        default_mode: c.default_mode,
        rtk_intensity: (($('c-engines').querySelector('[data-intensity]') || {}).value) || 'standard',
      };
      try {
        await api('/v1/compression', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
        $('c-msg').innerHTML = '<span class="s-ok">saved</span>';
      } catch { $('c-msg').innerHTML = '<span class="s-err">save failed</span>'; }
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
    const t = v.timeouts_ms || {};
    $('set-rows').innerHTML = [
      ['requests per minute', v.rate_rpm], ['min interval ms', v.rate_min_interval_ms],
      ['max concurrent per provider', v.rate_concurrent_requests], ['max queue wait ms', v.rate_max_wait_ms],
      ['compression default mode', v.compression_default_mode], ['api auth mode', v.api_auth],
      ['upstream timeout ms', t.request], ['connect timeout ms', t.connect],
      ['stream idle timeout ms', t.stream_idle], ['first-byte readiness ms', t.stream_readiness],
      ['readiness max ms', t.stream_readiness_max], ['SSE heartbeat ms', t.sse_heartbeat],
      ['disconnect grace ms', t.disconnect_grace],
    ].map(([k, val]) => `<tr><td>${esc(k)}</td><td>${esc(val ?? '-')}</td></tr>`).join('');
  },
};

// ── Analytics: combo health ──
PAGES.combohealth = {
  title: 'Combo Health',
  body: () => `
    <h1>Combo Health</h1>
    <p class="muted small">Success rate and latency per routing chain, from the request ring</p>
    <table><thead><tr><th>combo</th><th>strategy</th><th>members</th><th>requests</th><th>errors</th><th>success</th><th>avg ms</th></tr></thead><tbody id="ch-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/combo-health');
    $('ch-rows').innerHTML = v.combos.length
      ? v.combos.map((c) => `<tr>
          <td><b>${esc(c.combo)}</b></td><td><span class="badge">${esc(c.strategy)}</span></td>
          <td class="muted small">${esc((c.members || []).join(' → '))}</td>
          <td>${c.requests}</td><td class="${c.errors ? 's-err' : ''}">${c.errors}</td>
          <td class="${c.success_rate >= 99 ? 's-ok' : c.success_rate > 0 ? '' : 'muted'}">${c.success_rate}%</td>
          <td>${c.avg_latency_ms}</td></tr>`).join('')
      : '<tr><td colspan="7" class="muted small">no combos configured</td></tr>';
  },
};

// ── Analytics: utilization (rate-limit windows) ──
PAGES.utilization = {
  title: 'Utilization',
  body: () => `
    <h1>Utilization</h1>
    <p class="muted small">Per-provider rate-limit window usage and concurrency</p>
    <table><thead><tr><th>provider</th><th>window hits</th><th>budget</th><th>used</th><th>in-flight</th></tr></thead><tbody id="ut-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/quotas');
    $('ut-rows').innerHTML = v.quotas.map((q) => {
      const pct = q.rpmBudget ? Math.min(100, Math.round((q.rpmWindowHits / q.rpmBudget) * 100)) : 0;
      return `<tr><td>${esc(q.provider)}</td><td>${q.rpmWindowHits}</td><td>${q.rpmBudget}</td>
        <td><div style="background:var(--color-bg-alt);border-radius:4px;height:7px;width:130px;display:inline-block;vertical-align:middle">
          <div style="height:7px;border-radius:4px;width:${pct}%;background:${pct > 80 ? 'var(--bad)' : 'var(--color-accent)'}"></div></div>
        <span class="muted small"> ${pct}%</span></td>
        <td>${q.concurrent}</td></tr>`;
    }).join('');
  },
};

// ── Analytics: compression savings ──
PAGES.compressionstats = {
  title: 'Compression Analytics',
  body: () => `
    <h1>Compression</h1>
    <p class="muted small">Tokens saved by the compression engines over the request ring</p>
    <div class="cards" id="cs-cards"></div>
    <table><thead><tr><th>engine</th><th>requests</th><th>saved</th></tr></thead><tbody id="cs-rows"></tbody></table>`,
  after: async () => {
    const logs = (await api('/v1/logs?limit=500')).logs || [];
    const compressed = logs.filter((l) => l.compressed);
    const saved = logs.reduce((a, l) => a + (l.tokens_saved || 0), 0);
    const cfg = await api('/v1/compression').catch(() => null);
    $('cs-cards').innerHTML = [
      [compressed.length, 'requests with compression'],
      [logs.length, 'requests sampled'],
      [saved, 'tokens saved'],
      [cfg ? cfg.default_mode : '—', 'current default engine'],
    ].map(([n, l]) => `<div class="card"><div class="n">${esc(String(n))}</div><div class="l">${esc(l)}</div></div>`).join('');
    $('cs-rows').innerHTML = compressed.length
      ? compressed.slice(0, 50).map((l) => `<tr><td>${esc(l.model)}</td><td>1</td><td>${l.tokens_saved || 0}</td></tr>`).join('')
      : '<tr><td colspan="3" class="muted small">no compressed requests yet</td></tr>';
  },
};

// ── Analytics: provider stats (backend aggregates) ──
PAGES.providerstats = {
  title: 'Provider Stats',
  body: () => `
    <h1>Provider Stats</h1>
    <table><thead><tr><th>provider</th><th>format</th><th>requests</th><th>errors</th><th>success</th><th>avg ms</th><th>in / out tokens</th><th>cooldown</th></tr></thead><tbody id="ps-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/stats/providers');
    $('ps-rows').innerHTML = v.providers.length
      ? v.providers.map((p) => `<tr>
          <td>${esc(p.provider)}${p.has_key ? '' : ' <span class="badge">no key</span>'}</td>
          <td class="muted small">${esc(p.format)}</td><td>${p.requests}</td>
          <td class="${p.errors ? 's-err' : ''}">${p.errors}</td>
          <td class="${p.success_rate >= 99 ? 's-ok' : ''}">${p.success_rate}%</td>
          <td>${p.avg_latency_ms}</td>
          <td class="muted small">${p.prompt_tokens} / ${p.completion_tokens}</td>
          <td>${p.cooldown_ms > 0 ? `<span class="s-err">${p.cooldown_ms}ms</span>` : '0'}</td></tr>`).join('')
      : '<tr><td colspan="8" class="muted small">no requests recorded yet</td></tr>';
  },
};

// ── Analytics: free tiers (parity: /dashboard/free-tiers) ──
PAGES.freetiers = {
  title: 'Free tiers',
  body: () => `
    <h1>Free tiers</h1>
    <p class="muted small">Providers with a free tier, from the gateway catalog — connected ones first</p>
    <div class="filter-card"><div class="filter-row">
      <div class="search-wrap"><span class="material-symbols-outlined">search</span>
        <input type="search" id="ft-q" placeholder="Search free providers" autocomplete="off"></div>
      <div class="chip-row" id="ft-cats" style="margin-top:0"></div>
    </div></div>
    <div id="ft-summary" class="muted small" style="margin:10px 0"></div>
    <div id="ft-grid" class="card-grid4"><span class="muted">loading…</span></div>`,
  after: async () => {
    const v = await api('/v1/free-tiers').catch(() => ({ summary: {}, tiers: [] }));
    const tiers = v.tiers || [];
    const s = v.summary || {};
    let cat = 'all';
    const draw = () => {
      const q = ($('ft-q').value || '').toLowerCase().trim();
      const cats = ['all', ...Object.keys(s.byCategory || {})];
      $('ft-cats').innerHTML = cats.map((c) => {
        const n = c === 'all' ? tiers.length : (s.byCategory || {})[c];
        return `<button class="chip ${c === cat ? 'active' : ''}" data-cat="${esc(c)}">${esc(c)} <b>${n}</b></button>`;
      }).join('');
      $('ft-cats').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => { cat = b.dataset.cat; draw(); }));
      $('ft-summary').textContent = `${s.freeProviders || tiers.length} free-tier providers · ${s.connected || 0} connected`;
      const list = tiers.filter((t) => (cat === 'all' || t.category === cat)
        && (!q || ((t.name || '') + ' ' + t.provider).toLowerCase().includes(q)));
      $('ft-grid').innerHTML = list.length ? list.map((t) => `
        <div class="pcard2 ${t.connected ? 'connected' : ''}">
          <div class="pcard2-head">
            <span class="plogo2" style="color:${esc(t.color || '#888')};background:${esc(t.color || '#888')}15;border-radius:8px;width:34px;height:34px">
              <span class="material-symbols-outlined">${esc(t.icon || 'cloud')}</span></span>
            <div class="pcard2-name" title="${esc(t.name || t.provider)}">${esc(t.name || t.provider)}</div>
            <div class="pcard2-dots">${t.connected ? '<span class="cdot" style="background:var(--ok)"></span>' : ''}</div>
          </div>
          <div class="muted small">${esc(t.freeNote || 'Free tier available — see the provider site for current quotas.')}</div>
          <div class="pcard2-tags"><span class="tag info">${esc(t.category || 'apikey')}</span>
            ${(t.models || []).length ? `<span class="tag">${t.models.length} models</span>` : ''}</div>
          <div class="pcard2-foot">
            <span class="small ${t.connected ? 's-ok' : 'muted'}">${t.connected ? 'connected' : 'not connected'}</span>
            <span style="margin-left:auto;display:flex;gap:6px">
              ${t.website ? `<a class="mini" href="${esc(t.website)}" target="_blank" rel="noreferrer">site</a>` : ''}
              <button class="mini" data-open="${esc(t.provider)}">open in Providers</button>
            </span>
          </div>
        </div>`).join('') : '<div class="na-note">no free-tier providers match</div>';
      $('ft-grid').querySelectorAll('[data-open]').forEach((b) => b.addEventListener('click', () => {
        try {
          const u = new URL(location.href);
          u.searchParams.set('search', b.dataset.open);
          history.replaceState(history.state, '', u.toString());
        } catch {}
        setPage('providers');
      }));
    };
    $('ft-q').addEventListener('input', draw);
    draw();
  },
};

// ── Monitoring: audit log ──
PAGES.audit = {
  title: 'Audit Log',
  body: () => `
    <h1>Audit log</h1>
    <p class="muted small">Management actions (login, keys, providers, password, service)</p>
    <table><thead><tr><th>time</th><th>action</th><th>detail</th><th>result</th></tr></thead><tbody id="au-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/audit?limit=200');
    $('au-rows').innerHTML = v.audit.length
      ? v.audit.map((a) => `<tr><td>${new Date(a.ts_ms).toLocaleString()}</td><td><b>${esc(a.action)}</b></td>
          <td class="muted small">${esc(a.detail)}</td>
          <td>${a.ok ? '<span class="s-ok">ok</span>' : '<span class="s-err">denied</span>'}</td></tr>`).join('')
      : '<tr><td colspan="4" class="muted small">no management actions recorded yet</td></tr>';
  },
};

// ── Monitoring: log export ──
PAGES.logexport = {
  title: 'Log Export',
  body: () => `
    <h1>Log export</h1>
    <p class="muted small">Filter the request ring and download it</p>
    <div class="combo">
      <label>provider</label><input type="text" id="ex-provider" placeholder="(any)">
      <label>model contains</label><input type="text" id="ex-model" placeholder="(any)">
      <label>errors only</label><select id="ex-errors"><option value="false">no</option><option value="true">yes</option></select>
      <label>format</label><select id="ex-format"><option value="csv">csv</option><option value="json">json</option></select>
      <div style="margin-top:10px"><button class="save" id="ex-download">Download</button>
      <span id="ex-count" class="muted small" style="margin-left:10px"></span></div>
    </div>
    <h2>Preview</h2>
    <table><thead><tr><th>time</th><th>model</th><th>provider</th><th>status</th><th>in/out</th></tr></thead><tbody id="ex-rows"></tbody></table>`,
  after: async () => {
    const qs = () => {
      const p = [];
      if ($('ex-provider').value) p.push('provider=' + encodeURIComponent($('ex-provider').value));
      if ($('ex-model').value) p.push('model=' + encodeURIComponent($('ex-model').value));
      if ($('ex-errors').value === 'true') p.push('errors=true');
      return p.join('&');
    };
    const preview = async () => {
      const v = await api('/v1/logs?limit=50' + (qs() ? '&' + qs() : ''));
      $('ex-count').textContent = v.logs.length + ' rows (limit 50 preview)';
      $('ex-rows').innerHTML = v.logs.map((l) => `<tr><td>${new Date(l.ts_ms).toLocaleTimeString()}</td><td>${esc(l.model)}</td>
        <td>${esc(l.provider || '-')}</td><td>${statusBadge(l.status)}</td><td class="muted small">${l.prompt_tokens} / ${l.completion_tokens}</td></tr>`).join('');
    };
    ['ex-provider', 'ex-model', 'ex-errors'].forEach((id) => $(id).addEventListener('change', preview));
    $('ex-download').addEventListener('click', async () => {
      const url = '/v1/logs/export?limit=1000&format=' + $('ex-format').value + (qs() ? '&' + qs() : '');
      const r = await fetch(url, { headers: { authorization: 'Bearer ' + (localStorage.getItem('omniroute_session') || '') } });
      if (!r.ok) { toast('export failed: HTTP ' + r.status, false); return; }
      const blob = await r.blob();
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = 'omniroute-requests.' + $('ex-format').value;
      a.click();
      URL.revokeObjectURL(a.href);
      toast('export downloaded');
    });
    preview();
  },
};

// ── Tools: playground (real chat call through the gateway) ──
PAGES.playground = {
  title: 'Playground',
  body: () => `
    <h1>Playground</h1>
    <p class="muted small">Send a real request through this gateway (uses your configured providers)</p>
    <div class="combo">
      <label>model</label><select id="pg-model"></select>
      <label>stream</label><select id="pg-stream"><option value="false">no</option><option value="true">yes</option></select>
      <label>system</label><input type="text" id="pg-system" style="width:420px" placeholder="(optional)">
      <label>prompt</label><br><textarea id="pg-prompt" rows="4" style="width:100%;margin-top:6px"></textarea>
      <div style="margin-top:10px"><button class="save" id="pg-send">Send</button>
      <label style="margin-left:12px">compression</label><select id="pg-comp"><option value="">default</option><option value="off">off</option><option value="standard">standard</option><option value="aggressive">aggressive</option><option value="ultra">ultra</option><option value="rtk">rtk</option></select></div>
    </div>
    <h2>Response</h2>
    <div class="panel"><pre id="pg-out" style="white-space:pre-wrap;margin:0;font-size:12px">—</pre></div>`,
  after: async () => {
    const models = await api('/v1/models').catch(() => ({ data: [] }));
    $('pg-model').innerHTML = (models.data || []).map((m) => `<option value="${esc(m.id)}">${esc(m.id)}</option>`).join('') || '<option value="">no models</option>';
    $('pg-prompt').value = 'Reply with exactly: pong';
    $('pg-send').addEventListener('click', async () => {
      const body = {
        model: $('pg-model').value,
        messages: [
          ...($('pg-system').value ? [{ role: 'system', content: $('pg-system').value }] : []),
          { role: 'user', content: $('pg-prompt').value },
        ],
        stream: $('pg-stream').value === 'true',
      };
      const headers = { 'content-type': 'application/json', authorization: 'Bearer ' + (localStorage.getItem('omniroute_session') || '') };
      if ($('pg-comp').value) headers['x-omniroute-compression'] = $('pg-comp').value;
      $('pg-out').textContent = '…';
      const t0 = performance.now();
      try {
        if (body.stream) {
          const r = await fetch('/v1/chat/completions', { method: 'POST', headers, body: JSON.stringify(body) });
          const reader = r.body.getReader();
          const dec = new TextDecoder();
          let acc = '';
          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;
            acc += dec.decode(value, { stream: true });
            $('pg-out').textContent = acc.slice(-4000);
          }
        } else {
          const r = await fetch('/v1/chat/completions', { method: 'POST', headers, body: JSON.stringify(body) });
          const j = await r.json();
          const txt = j?.choices?.[0]?.message?.content ?? JSON.stringify(j, null, 2);
          $('pg-out').textContent = `${txt}\n\n— ${Math.round(performance.now() - t0)}ms · usage ${JSON.stringify(j.usage || {})} · compression ${r.headers.get('x-omniroute-compression') || 'n/a'}`;
        }
      } catch (e) { $('pg-out').textContent = 'error: ' + e; }
    });
  },
};

// ── Tools: translator (format docs + live conversion check) ──
PAGES.translator = {
  title: 'Translator',
  body: () => `
    <h1>Translator</h1>
    <p class="muted small">The gateway translates between wire formats automatically; the table below shows the matrix it serves</p>
    <table><thead><tr><th>inbound</th><th>anthropic</th><th>openai</th><th>gemini</th><th>responses</th></tr></thead><tbody id="tr-rows"></tbody></table>
    <h2>Probe</h2>
    <div class="combo">
      <label>send a claude-shaped request</label>
      <div><button class="save" id="tr-probe">POST /v1/messages</button></div>
      <pre id="tr-out" style="white-space:pre-wrap;font-size:12px;margin-top:10px">—</pre>
    </div>`,
  after: async () => {
    const cells = ['✓', '✓', '✓', '✓'];
    $('tr-rows').innerHTML = ['anthropic /v1/messages', 'openai /v1/chat/completions', 'gemini generateContent', 'openai /v1/responses']
      .map((r) => `<tr><td>${esc(r)}</td>${cells.map((c) => `<td class="s-ok">${c}</td>`).join('')}</tr>`).join('');
    $('tr-probe').addEventListener('click', async () => {
      const model = (await api('/v1/models').catch(() => ({ data: [] }))).data?.[0]?.id || '';
      const r = await fetch('/v1/messages', {
        method: 'POST',
        headers: { 'content-type': 'application/json', authorization: 'Bearer ' + (localStorage.getItem('omniroute_session') || '') },
        body: JSON.stringify({ model, max_tokens: 16, messages: [{ role: 'user', content: 'ping' }] }),
      });
      $('tr-out').textContent = `HTTP ${r.status}\n` + (await r.text()).slice(0, 1200);
    });
  },
};

// ── Tools: batch ──
PAGES.batch = {
  title: 'Batch',
  body: () => `
    <h1>Batch</h1>
    <p class="muted small">Batch API passthrough: create with POST /v1/batches (provider chosen via the <code>x-omniroute-provider</code> header), then poll by id</p>
    <div class="combo">
      <label>batch id</label><input type="text" id="bt-id" placeholder="batch_...">
      <div><button class="save" id="bt-get">Fetch status</button></div>
      <pre id="bt-out" style="white-space:pre-wrap;font-size:12px;margin-top:10px">—</pre>
    </div>`,
  after: async () => {
    $('bt-get').addEventListener('click', async () => {
      try {
        const v = await api('/v1/batches/' + encodeURIComponent($('bt-id').value));
        $('bt-out').textContent = JSON.stringify(v, null, 2);
      } catch (e) { $('bt-out').textContent = String(e); }
    });
  },
};

// ── Configuration: appearance ──
PAGES.appearance = {
  title: 'Settings · Appearance',
  body: () => `
    <h1>Settings · Appearance</h1>
    <h2>Theme</h2>
    <div class="combo">
      <label>theme</label><select id="ap-theme"><option value="dark">dark</option><option value="light">light</option></select>
      <div class="muted small" style="margin-top:6px">Also available from the topbar sun/moon button.</div>
    </div>
    <h2>Language</h2>
    <div class="combo">
      <label>locale</label><select id="ap-locale"></select>
      <div class="muted small" style="margin-top:6px">66 locales, message packs copied from the upstream <code>src/i18n/messages</code>.</div>
    </div>`,
  after: async () => {
    $('ap-theme').value = document.documentElement.dataset.theme || 'dark';
    $('ap-theme').addEventListener('change', () => {
      $('theme-toggle').click();
      $('ap-theme').value = document.documentElement.dataset.theme;
    });
    const langs = await (await fetch('/dashboard/languages.json')).json();
    $('ap-locale').innerHTML = langs.map((l) => `<option value="${esc(l.code)}">${esc(l.flag || '')} ${esc(l.native || l.name)}</option>`).join('');
    $('ap-locale').value = localStorage.getItem('omniroute_locale') || 'en';
    $('ap-locale').addEventListener('change', async () => {
      await loadPack($('ap-locale').value);
      localStorage.setItem('omniroute_locale', $('ap-locale').value);
      document.cookie = 'omniroute_locale=' + $('ap-locale').value + '; Path=/dashboard; Max-Age=31536000; SameSite=Lax';
      buildSidebar($('nav-search').value);
      setPage('appearance');
    });
  },
};

// ── Configuration: sidebar visibility ──
PAGES.sidebarsettings = {
  title: 'Settings · Sidebar',
  body: () => `
    <h1>Settings · Sidebar</h1>
    <p class="muted small">Sections and items shown in the navigation (persisted per browser)</p>
    <div id="sb-toggles"></div>`,
  after: async () => {
    const hidden = new Set(JSON.parse(localStorage.getItem('omniroute_hidden_nav') || '[]'));
    const rows = [];
    NAV.forEach((sec) => {
      if (sec.grp) return;
      rows.push(`<div class="combo"><b>${esc(label(sec.k, sec.title) || '(top)')}</b><div style="margin-top:8px">`);
      sec.items.forEach((it) => {
        if (it.grp || !it.p) return;
        const on = !hidden.has(it.id);
        rows.push(`<label style="display:inline-flex;align-items:center;gap:6px;min-width:230px;color:var(--color-text-main)">
          <input type="checkbox" data-nav-id="${esc(it.id)}" ${on ? 'checked' : ''}> ${esc(label(it.k, it.label))}</label>`);
      });
      rows.push('</div></div>');
    });
    $('sb-toggles').innerHTML = rows.join('');
    $('sb-toggles').querySelectorAll('input[data-nav-id]').forEach((cb) => cb.addEventListener('change', () => {
      const h = new Set(JSON.parse(localStorage.getItem('omniroute_hidden_nav') || '[]'));
      if (cb.checked) h.delete(cb.dataset.navId); else h.add(cb.dataset.navId);
      localStorage.setItem('omniroute_hidden_nav', JSON.stringify([...h]));
      buildSidebar($('nav-search').value);
      toast('sidebar updated');
    }));
  },
};

// ── Configuration: resilience (live rate limits) ──
PAGES.resilience = {
  title: 'Settings · Resilience',
  body: () => `
    <h1>Settings · Resilience</h1>
    <p class="muted small">Cooldown state and live rate-limit settings</p>
    <table><thead><tr><th>provider</th><th>cooldown</th><th>in-flight</th><th>window hits</th></tr></thead><tbody id="rs-rows"></tbody></table>
    <h2>Rate limits (read from config)</h2>
    <table><thead><tr><th>metric</th><th>value</th></tr></thead><tbody id="rs-limits"></tbody></table>`,
  after: async () => {
    const q = await api('/v1/quotas');
    $('rs-rows').innerHTML = q.quotas.map((x) => `<tr><td>${esc(x.provider)}</td>
      <td>${x.cooldownMs > 0 ? `<span class="s-err">${x.cooldownMs}ms</span>` : '<span class="s-ok">0</span>'}</td>
      <td>${x.concurrent}</td><td>${x.rpmWindowHits}/${x.rpmBudget}</td></tr>`).join('');
    const s = await api('/v1/settings');
    $('rs-limits').innerHTML = [
      ['requests per minute', s.rate_rpm],
      ['min interval ms', s.rate_min_interval_ms],
      ['max concurrent per provider', s.rate_concurrent_requests],
      ['max queue wait ms', s.rate_max_wait_ms],
    ].map(([k, v]) => `<tr><td>${esc(k)}</td><td>${esc(String(v))}</td></tr>`).join('');
  },
};

// ── Configuration: security ──
PAGES.security = {
  title: 'Settings · Security',
  body: () => `
    <h1>Settings · Security</h1>
    <h2>Admin password</h2>
    <div class="combo">
      <label>current password</label><input type="password" id="sec-cur" style="width:280px">
      <label>new password (min 8)</label><input type="password" id="sec-new" style="width:280px">
      <div style="margin-top:8px"><button class="save" id="sec-save">Change password</button> <span id="sec-msg" class="small"></span></div>
      <div class="muted small" style="margin-top:8px">Forgot it? Run <code>omniroute reset-password --password '&lt;new&gt;'</code> — applies immediately, no restart.</div>
    </div>
    <h2>Authentication</h2>
    <table><thead><tr><th>item</th><th>value</th></tr></thead><tbody id="sec-rows"></tbody></table>`,
  after: async () => {
    const [me, s] = await Promise.all([api('/v1/auth/me'), api('/v1/settings')]);
    $('sec-rows').innerHTML = [
      ['authenticated', String(me.authenticated)],
      ['session method', me.method],
      ['using default password', String(me.using_default_password)],
      ['inference auth mode', s.api_auth],
      ['manage sessions/keys', 'API Manager tab'],
    ].map(([k, v]) => `<tr><td>${esc(k)}</td><td>${esc(String(v))}</td></tr>`).join('');
    $('sec-save').addEventListener('click', async () => {
      try {
        await api('/v1/auth/change-password', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ current_password: $('sec-cur').value, new_password: $('sec-new').value }) });
        $('sec-msg').innerHTML = '<span class="s-ok">updated</span>';
        toast('password changed');
      } catch (e) {
        $('sec-msg').innerHTML = '<span class="s-err">failed — check the current password</span>';
      }
    });
  },
};

// ── Combos Studio: live routing view ──
PAGES.combostudio = {
  title: 'Combos Studio',
  body: () => `
    <h1>Combos Studio</h1>
    <p class="muted small">Live routing view — each combo with its resolved candidate chain, current selection and circuit state</p>
    <div class="filter-row" style="margin-bottom:12px">
      <div class="search-wrap"><span class="material-symbols-outlined">route</span>
        <input type="search" id="cs-model" placeholder="dry-run a model, e.g. coding" autocomplete="off"></div>
      <button class="mini" id="cs-run"><span class="material-symbols-outlined" style="font-size:15px;vertical-align:-3px">play_arrow</span> Resolve chain</button>
    </div>
    <div id="cs-dry" class="na-note" style="display:none"></div>
    <div id="cs-list"><span class="muted">loading…</span></div>`,
  after: async () => {
    const draw = (combos) => {
      $('cs-list').innerHTML = combos.length ? combos.map((c) => `
        <div class="section-card">
          <div class="section-head">
            <span class="material-symbols-outlined" style="color:${iconAccent(c.combo)}">${c.healthy ? 'check_circle' : 'error'}</span>
            <div><h3>${esc(c.combo)}</h3>
              <div class="muted small">${esc(c.selected ? 'selecting ' + c.selected : 'no healthy candidate')} · ${c.candidates.length} candidates</div></div>
            <span class="status-pill ${c.healthy ? 'healthy' : 'critical'}"><i></i>${c.healthy ? 'routing' : 'degraded'}</span>
          </div>
          <table style="margin-top:10px"><thead><tr><th>#</th><th>provider</th><th>model</th><th>state</th><th>key</th><th>in-flight</th><th>cooldown</th></tr></thead><tbody>
          ${c.candidates.map((x, i) => `<tr>
            <td>${i + 1}</td><td>${esc(x.provider)}</td><td class="muted small">${esc(x.model)}</td>
            <td>${x.available && !x.modelBanned ? '<span class="s-ok">available</span>' : `<span class="s-err">${x.modelBanned ? 'model banned' : 'cooling'}</span>`}</td>
            <td>${x.hasKey ? '<span class="material-symbols-outlined flag-key" style="font-size:14px">key</span>' : '<span class="muted small">none</span>'}</td>
            <td>${x.inFlight}</td><td>${x.cooldownMs > 0 ? x.cooldownMs + 'ms' : '0'}</td></tr>`).join('')}
          </tbody></table>
        </div>`).join('') : '<div class="na-note">no combos configured</div>';
    };
    const load = async () => {
      const v = await api('/v1/combo-studio').catch(() => ({ combos: [] }));
      draw(v.combos || []);
    };
    $('cs-run').addEventListener('click', async () => {
      const model = $('cs-model').value.trim();
      if (!model) return;
      const r = await fetch('/v1/combos/test', {
        method: 'POST',
        headers: { 'content-type': 'application/json', authorization: 'Bearer ' + (localStorage.getItem('omniroute_session') || '') },
        body: JSON.stringify({ model }),
      });
      const v = await r.json().catch(() => ({}));
      $('cs-dry').style.display = 'block';
      $('cs-dry').innerHTML = `<b>${esc(model)}</b> → ` + ((v.candidates || []).map((c, i) =>
        `${i + 1}. ${esc(c.provider)}${c.available ? '' : ' (unavailable)'}`).join(' → ') || 'no candidates');
    });
    await load();
  },
};

// ── Embedded services ──
PAGES.embeddedservices = {
  title: 'Embedded Services',
  body: () => `
    <h1>Embedded services</h1>
    <p class="muted small">Local execution surfaces bundled with the gateway</p>
    <h2>Local inference providers</h2>
    <div id="es-local" class="card-grid"></div>
    <h2>Bundled executors</h2>
    <table><thead><tr><th>executor</th><th>description</th><th>status</th></tr></thead><tbody id="es-exec"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/embedded-services').catch(() => null);
    if (!v) { $('es-local').innerHTML = '<div class="na-note">unavailable</div>'; return; }
    $('es-local').innerHTML = (v.localProviders || []).length
      ? v.localProviders.map((p) => `<div class="pcard">
          <div class="pcard-head"><span class="plogo" style="background:${esc(p.id)}22;color:#38d39f"><span class="material-symbols-outlined">memory</span></span>
            <div class="pcard-name">${esc(p.id)}</div>
            <div class="pcard-flags"><span class="dot ${p.cooldownMs > 0 ? '' : 'ok'}"></span></div></div>
          <div class="chain">${esc(p.baseUrl || 'registry default')}</div>
          <div class="pcard-foot"><span class="muted small">${p.hasKey ? 'key set' : 'no key'} · ${p.inFlight} in-flight</span></div>
        </div>`).join('')
      : '<div class="na-note">no local providers registered — add ollama/lmstudio to your config</div>';
    $('es-exec').innerHTML = (v.bundledExecutors || []).map((e) => `<tr>
      <td>${esc(e.id)}</td><td class="muted small">${esc(e.description)}</td>
      <td>${e.available ? '<span class="s-ok">available</span>' : `<span class="muted small">${esc(e.reason || 'not available')}</span>`}</td></tr>`).join('');
  },
};

// ── Quota share ──
PAGES.quotashare = {
  title: 'Quota Share',
  body: () => `
    <h1>Quota share</h1>
    <p class="muted small">How each provider's budget is shared across your API keys</p>
    <div class="cards" id="qs-cards"></div>
    <table><thead><tr><th>provider</th><th>shared rpm</th><th>keys in pool</th><th>advisory per-key rpm</th><th>window hits</th><th>concurrent</th><th>tier</th></tr></thead><tbody id="qs-rows"></tbody></table>
    <div id="qs-note" class="na-note" style="margin-top:12px"></div>`,
  after: async () => {
    const v = await api('/v1/quota-share').catch(() => null);
    if (!v) { $('qs-rows').innerHTML = '<tr><td colspan="7" class="muted small">unavailable</td></tr>'; return; }
    $('qs-cards').innerHTML = [
      [v.totalKeys, 'registered keys'], [v.enabledKeys, 'enabled keys'], [(v.shares || []).length, 'shared providers'],
    ].map(([n, l]) => `<div class="card"><div class="n">${n}</div><div class="l">${esc(l)}</div></div>`).join('');
    $('qs-rows').innerHTML = (v.shares || []).length ? v.shares.map((s) => `<tr>
      <td>${esc(s.provider)}</td><td>${s.sharedRpm}</td><td>${s.keysInPool}</td><td>${s.perKeyRpm}</td>
      <td>${s.windowHits}</td><td>${s.inFlight}/${s.concurrent}</td><td><span class="tag">${esc(s.tier)}</span></td></tr>`).join('')
      : '<tr><td colspan="7" class="muted small">no quota overrides yet — set one on the Provider quota page</td></tr>';
    $('qs-note').textContent = v.note || '';
  },
};

// ── Route tracing ──
PAGES.routingtrace = {
  title: 'Route Tracing',
  body: () => `
    <h1>Route tracing</h1>
    <p class="muted small">Recent requests with the candidate chain that was resolved for them</p>
    <table><thead><tr><th>time</th><th>model</th><th>served by</th><th>position</th><th>chain</th><th>status</th><th>ms</th></tr></thead><tbody id="rt2-rows"></tbody></table>`,
  after: async () => {
    const v = await api('/v1/routing/trace?limit=100').catch(() => ({ traces: [] }));
    $('rt2-rows').innerHTML = (v.traces || []).length ? v.traces.map((t) => `<tr>
      <td>${new Date(t.ts_ms).toLocaleTimeString()}</td>
      <td>${esc(t.model)}</td>
      <td>${esc(t.provider || '-')}${t.fallback ? ' <span class="tag warn">fallback</span>' : ''}</td>
      <td>${t.served_position ? t.served_position + '/' + t.chain_len : '—'}</td>
      <td class="muted small">${esc((t.candidate_chain || []).map((c) => c.provider).join(' → ') || 'direct')}</td>
      <td>${statusBadge(t.status)}</td><td>${t.latency_ms}</td></tr>`).join('')
      : '<tr><td colspan="7" class="muted small">no requests recorded yet</td></tr>';
  },
};

// ── Cache health ──
PAGES.cachehealth = {
  title: 'Cache Health',
  body: () => `
    <h1>Cache health</h1>
    <p class="muted small">Semantic cache and dedup/compression effectiveness</p>
    <div class="cards" id="ch2-cards"></div>
    <div id="ch2-note" class="na-note" style="margin-top:12px"></div>`,
  after: async () => {
    const v = await api('/v1/cache/health').catch(() => null);
    if (!v) { $('ch2-cards').innerHTML = '<div class="na-note">unavailable</div>'; return; }
    $('ch2-cards').innerHTML = [
      [v.semanticCache?.hits ?? 0, 'cache hits'],
      [v.semanticCache?.misses ?? 0, 'cache misses'],
      [v.dedup?.requestsCompressed ?? 0, 'requests compressed'],
      [`${v.dedup?.savedRatioPct ?? 0}%`, 'prompt tokens saved'],
    ].map(([n, l]) => `<div class="card"><div class="n">${esc(String(n))}</div><div class="l">${esc(l)}</div></div>`).join('');
    $('ch2-note').textContent = (v.semanticCache?.reason || '') + (v.dedup ? ` · ${v.dedup.tokensSaved} tokens saved by compression` : '');
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
// The session cookie (Path=/) rides along automatically; the stored Bearer
// token is sent as well so a refresh keeps the login (original parity).
async function bootAuth() {
  try {
    const tok = localStorage.getItem('omniroute_session') || '';
    const r = await fetch('/v1/auth/me', tok ? { headers: { authorization: 'Bearer ' + tok } } : {});
    const me = await r.json();
    if (!me.authenticated && tok) localStorage.removeItem('omniroute_session');
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
