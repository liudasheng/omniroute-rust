# Page audit: original dashboard vs omniroute-rust

Reference: `diegosouzapw/OmniRoute` v3.8.x, nav source of truth
`src/shared/constants/sidebarVisibility/sections.ts` (~93 entries in 10
sections). The embedded SPA (`src/server/dashboard_assets/app.js`) now mirrors
that file 1:1 — same sections, groups, order, icons and i18n keys — with
`PAGES.*` registrations for every non-external entry (76 pages). Audited
2026-09 against `/tmp/omniroute-src` (first ~60 lines of each `page.tsx`) and
the embedded SPA. 中文版见 [zh/PAGES.md](zh/PAGES.md)。

Status: **done** = data-backed page · **partial** = covered by another page,
static replica, or read-only · **stub** = registered page rendering the
original's visual shell with an honest "not ported to Rust yet" empty state
(never fake data) · **redirect** = redirect-only alias in the original too.

## Home / OmniProxy

| Original route | Rust page | Status | Notes |
|---|---|---|---|
| `/home` | Home | done | standalone Home sidebar item, quick-start, 2fr/1fr connected-provider topology + recent-request layout, 3s refresh; provider nodes open editable detail |
| `/dashboard/endpoint` | Endpoints | done | active endpoints, local server, per-endpoint model counts, custom system prompt, honest tunnel/alias states |
| `/dashboard/api-manager` | API Manager | done | key CRUD + rotate, filters, usage/cost columns |
| `/dashboard/providers` | Providers | done | 352 catalog, 18 sections, compatible nodes, batch test + results, detail view, URL filters, sync/delete model management with deduplicated capability cards |
| `/dashboard/providers/services` | Embedded services | done | local executors reported unavailable, never faked |
| `/dashboard/combos` | Combos | done | builder wizard, kimi/auto-combo presets, model search, dry-run test; weights not persisted yet (honest fallback to even shares) |
| `/dashboard/combos/live` | Combos Studio | done | live routing view |
| `/dashboard/quota` | Provider Quota | done | cutoffs/balances/tiers, severity |
| `/dashboard/costs/quota-share` | Quota share | done | budget across keys |
| `/dashboard/context/settings\|combos\|caveman\|rtk\|headroom\|lite\|aggressive\|ultra` | Compression | done | runtime editor + per-engine views (multiple sidebar entries alias the same page) |
| `/dashboard/context/session-dedup\|ccr\|llmlingua\|omniglyph`, `/compression/studio\|exclusions` | Compression (aliases) | partial | sidebar entries registered, all route to the Compression page; per-engine views remain a gap (see PARITY §6: only lite/standard/aggressive/ultra/rtk) |
| `/dashboard/cli-code` | CLI Code | partial | static replica of the upstream tool catalog (code tools with base-URL support); no detection/profile-sync hooks |
| `/dashboard/cli-agents`, `/acp-agents`, `/cloud-agents`, `/conductor`, `/orchestration`, `/tools/agent-bridge` | stub pages | stub | agent runtimes/fleets need executors the Rust gateway does not ship |
| `/dashboard/tools/traffic-inspector` | Logs (row-detail drawer) | done | full-entry modal from the request ring |
| `/dashboard/discovery` | Discovery | stub | model scan needs upstream catalog sync (registry is static + connections) |
| `/dashboard/api-endpoints` | API Endpoints | stub | listing parity exists on the Endpoints page; no OpenAPI try-it console |
| `/dashboard/webhooks` | Webhooks | stub | webhook editor needs a delivery engine |
| `/dashboard/log-export` | Log export | done | CSV/JSON + destinations status |
| `/dashboard/system/proxy` | Proxy | stub | upstream mitm proxy subsystem, out of scope |

## Analytics / Costs / Monitoring

| Original route | Rust page | Status | Notes |
|---|---|---|---|
| `/dashboard/analytics` (hub) | Usage | partial | overview analytics; evals/search tabs are gaps (below) |
| `/dashboard/analytics/combo-health` | Combo Health | done | |
| `/dashboard/analytics/utilization` | Utilization | done | |
| `/dashboard/cache` | Cache Health | done | dedup stats, honest semantic-cache disabled state |
| `/dashboard/analytics/compression` | Compression analytics | done | tokens-saved from the ring |
| `/dashboard/analytics/search` | Search analytics | stub | needs search/RAG accounting |
| `/dashboard/analytics/evals` | Evals | stub | needs an eval harness |
| `/dashboard/provider-stats` | Provider Stats | done | per-provider counters + cooldowns |
| `/dashboard/costs` | Costs | stub | no cost accounting in the ring (`/v1/usage/analytics` carries tokens only) |
| `/dashboard/costs/pricing` | Pricing | stub | needs a pricing table |
| `/dashboard/costs/budget` | Budget | stub | no spend limits subsystem |
| `/dashboard/free-tiers` | Free tiers | done | 151-entry catalog + connection state; no summed token headline (cannot verify pool budgets) |
| `/dashboard/free-provider-rankings` | Free provider rankings | stub | curated task-fit rankings, editorial content |
| `/dashboard/radar` | Route tracing | partial | routing decisions; radar catalog/referrals are sponsor chrome |
| `/dashboard/activity` | Logs | partial | aliased to the request ring (no separate compliance feed) |
| `/dashboard/logs` | Logs | done | ring + full-entry detail drawer |
| `/dashboard/logs/proxy` | Proxy logs | stub | no mitm traffic in the Rust gateway |
| `/dashboard/logs/console` | Console logs | stub | no service-output capture |
| `/dashboard/logs/timeline` | Timeline | stub | metadata-only ring, no per-turn timeline |
| `/dashboard/conversations` | Conversations | stub | no turn bodies stored (metadata-only ring) |
| `/dashboard/audit` | Audit log | done | management-action ring |
| `/dashboard/audit/mcp` | MCP audit | stub | no MCP runtime |
| `/dashboard/audit/a2a` | A2A audit | stub | no A2A runtime |
| `/dashboard/health` | Health | done | probes + compression/auth states |
| `/dashboard/runtime` | Runtime | done | pid/uptime/RSS/counters |
| `/dashboard/resilience/connections` | Resilience (+ Settings·Resilience) | done | cooldowns, rate windows, breaker profiles |

## Dev Tools / Agentic / Other / Configuration

| Original route | Rust page | Status | Notes |
|---|---|---|---|
| `/dashboard/translator` | Translator | done | format docs + live conversion check |
| `/dashboard/playground` | Playground | done | sends real chat through the gateway |
| `/dashboard/search-tools` | Search tools | stub | needs keyed search providers (passthrough exists, no workbench) |
| `/dashboard/memory`, `/agent-skills`, `/omni-skills`, `/mcp`, `/a2a`, `/plugins` | stub pages | stub | memory/MCP/A2A/skill runtimes out of scope |
| `/dashboard/chaos` | Chaos Mode | stub | parallel-run chaos needs a fan-out executor; `chaos_mode_enabled` is stored on keys but unenforced |
| `/dashboard/leaderboard`, `/profile`, `/tokens`, `/gamification/admin` | stub pages | stub | gamification chrome, deliberately excluded |
| `/dashboard/cache/media` | Media | stub | media-provider pipelines out of scope |
| `/dashboard/batch` | Batch | done | batch/file status via `provider/model` routing |
| `/dashboard/batch/files` | Batch files | partial | Files API passthrough documented (`POST /v1/files`); no local file registry to list |
| `/dashboard/settings/general` | Settings·General | done | limits, auth, upstream timeouts (read-only) |
| `/dashboard/settings/appearance` | Settings·Appearance | done | theme + language |
| `/dashboard/settings/sidebar` | Settings·Sidebar | done | per-browser nav visibility, includes new pages automatically |
| `/dashboard/settings/resilience` | Settings·Resilience | done | cooldown profiles |
| `/dashboard/settings/security` | Settings·Security | done | password + auth |
| `/dashboard/settings/ai` | Settings·AI | stub | AI runtimes need the TS executors |
| `/dashboard/settings/modality-bridge` | Settings·Modality bridge | stub | image/audio bridging out of scope |
| `/dashboard/settings/routing` | Global Routing | stub | `/v1/settings` carries no routing fields; routing lives on Combos/Quota pages |
| `/dashboard/settings/advanced` | Settings·Advanced | stub | executor flags need the TS runtimes |
| `/dashboard/settings/access-tokens` | Access Tokens | stub | API Manager covers key CRUD; no personal-token subsystem |
| `/dashboard/settings/feature-flags` | Settings·Feature flags | stub | no feature-flag store |
| `/dashboard/settings/cache` | Settings·Cache | stub | cache tuning not exposed |
| `/dashboard/changelog` | Changelog | partial | static pointer to the upstream releases page (repo ships no CHANGELOG) |
| `/docs` (external) | Docs nav link | done | points at the upstream repo |
| Issues (external) | Issues nav link | done | upstream GitHub issues, added to Help |

Redirect-only aliases in the original (no UI on either side):
`auto-combo`, `compression` (bare), `context` (bare), `limits`,
`settings` (bare), `usage` (bare → logs). Sub-route-only groups with no
bare page on either side: `gamification`, `resilience`, `system`, `tools`.
Unlisted but functional in the original with no Rust equivalent: `relay`
(token CRUD), `onboarding` (covered by the providers onboarding wizard +
first-run hint), `media-providers/[kind]`.
