# Page audit: original dashboard vs omniroute-rust

Reference: `diegosouzapw/OmniRoute` v3.8.x, nav source of truth
`src/shared/constants/sidebarVisibility/sections.ts` (~93 entries in 10
sections). Audited 2026-09 against `/tmp/omniroute-src` (first ~60 lines of
each `page.tsx`) and the embedded SPA (`src/server/dashboard_assets/app.js`,
32 `PAGES.*`). 中文版见 [zh/PAGES.md](zh/PAGES.md)。

Status: **done** = data-backed page · **partial** = covered by another page
or read-only · **gap** = no Rust equivalent (reason given) ·
**redirect** = redirect-only alias in the original too.

## Home / OmniProxy

| Original route | Rust page | Status | Notes |
|---|---|---|---|
| `/home` | Home | done | quick-start, provider topology, recent requests |
| `/dashboard/endpoint` | Endpoints | done | active endpoints, local server, per-endpoint model counts, custom system prompt, honest tunnel/alias states |
| `/dashboard/api-manager` | API Manager | done | key CRUD + rotate, filters, usage/cost columns |
| `/dashboard/providers` | Providers | done | 352 catalog, 18 sections, compatible nodes, batch test + results, detail view, URL filters |
| `/dashboard/providers/services` | Embedded services | done | local executors reported unavailable, never faked |
| `/dashboard/combos` | Combos | done | builder, weights, validation, test |
| `/dashboard/combos/live` | Combos Studio | done | live routing view |
| `/dashboard/quota` | Provider Quota | done | cutoffs/balances/tiers, severity |
| `/dashboard/costs/quota-share` | Quota share | done | budget across keys |
| `/dashboard/context/settings|combos|caveman|rtk|headroom|lite|aggressive|ultra` | Compression + engine pages | done | runtime editor + per-engine views |
| `/dashboard/context/session-dedup|ccr|llmlingua|omniglyph`, `/compression/studio|exclusions` | — | gap | engines out of scope (see PARITY §6: only lite/standard/aggressive/ultra/rtk) |
| `/dashboard/cli-code`, `/cli-agents`, `/acp-agents`, `/cloud-agents`, `/conductor`, `/orchestration`, `/tools/agent-bridge` | — | gap | agent runtimes/fleets need executors the Rust gateway does not ship |
| `/dashboard/tools/traffic-inspector` | Logs (row-detail drawer) | done | full-entry modal from the request ring |
| `/dashboard/discovery` | — | gap | model scan needs upstream catalog sync (registry is static + connections) |
| `/dashboard/api-endpoints` | Endpoints | partial | listing parity; no OpenAPI try-it console |
| `/dashboard/webhooks` | — | gap | webhook editor needs a delivery engine |
| `/dashboard/log-export` | Log export | done | CSV/JSON + destinations status |
| `/dashboard/system/proxy` | — | gap | upstream mitm proxy subsystem, out of scope |

## Analytics / Costs / Monitoring

| Original route | Rust page | Status | Notes |
|---|---|---|---|
| `/dashboard/analytics` (hub) | Usage | partial | overview analytics; evals/search tabs are gaps (below) |
| `/dashboard/analytics/combo-health` | Combo Health | done | |
| `/dashboard/analytics/utilization` | Utilization | done | |
| `/dashboard/cache` | Cache Health | done | dedup stats, honest semantic-cache disabled state |
| `/dashboard/analytics/compression` | Compression analytics | done | tokens-saved from the ring |
| `/dashboard/analytics/search`, `/analytics/evals` | — | gap | need search/RAG + eval harnesses |
| `/dashboard/provider-stats` | Provider Stats | done | per-provider counters + cooldowns |
| `/dashboard/costs`, `/costs/pricing`, `/costs/budget` | — | gap | needs a pricing table (no `costs/*` accounting) |
| `/dashboard/free-tiers` | Free tiers | done | 151-entry catalog + connection state; no summed token headline (cannot verify pool budgets) |
| `/dashboard/free-provider-rankings` | — | gap | curated task-fit rankings, editorial content |
| `/dashboard/radar` | Route tracing | partial | routing decisions; radar catalog/referrals are sponsor chrome |
| `/dashboard/activity` | Logs | partial | aliased to the request ring (no separate compliance feed) |
| `/dashboard/logs`, `/logs/proxy|console|timeline`, `/conversations` | Logs | partial | ring + detail; no proxy/console/timeline split, no turn bodies (metadata-only ring) |
| `/dashboard/audit` (+`/mcp`, `/a2a`) | Audit log | done | management-action ring (no MCP/A2A sides) |
| `/dashboard/health` | Health | done | probes + compression/auth states |
| `/dashboard/runtime` | Runtime | done | pid/uptime/RSS/counters |
| `/dashboard/resilience/connections` | Resilience (+ Settings·Resilience) | done | cooldowns, rate windows, breaker profiles |

## Dev Tools / Agentic / Other / Configuration

| Original route | Rust page | Status | Notes |
|---|---|---|---|
| `/dashboard/translator` | Translator | done | format docs + live conversion check |
| `/dashboard/playground` | Playground | done | sends real chat through the gateway |
| `/dashboard/search-tools` | — | gap | needs keyed search providers (passthrough exists, no workbench) |
| `/dashboard/memory`, `/agent-skills`, `/omni-skills`, `/mcp`, `/a2a`, `/plugins` | — | gap | memory/MCP/A2A/skill runtimes out of scope |
| `/dashboard/chaos` | — | gap | parallel-run chaos needs a fan-out executor; `chaos_mode_enabled` is stored on keys but unenforced |
| `/dashboard/leaderboard`, `/profile`, `/tokens` | — | gap | gamification chrome, deliberately excluded |
| `/dashboard/cache/media` | — | gap | media-provider pipelines out of scope |
| `/dashboard/batch` | Batch | done | batch/file status via `provider/model` routing |
| `/dashboard/settings/general` | Settings·General | done | limits, auth, upstream timeouts (read-only) |
| `/dashboard/settings/appearance` | Settings·Appearance | done | theme + language |
| `/dashboard/settings/sidebar` | Settings·Sidebar | done | per-browser nav visibility, includes new pages automatically |
| `/dashboard/settings/resilience` | Settings·Resilience | done | cooldown profiles |
| `/dashboard/settings/security` | Settings·Security | done | password + auth |
| `/dashboard/settings/ai|modality-bridge|routing|advanced|access-tokens|feature-flags|cache` | — | gap | AI/modality/executor settings need the TS runtimes; access-tokens ≈ API Manager |
| `/dashboard/changelog` | — | gap | news viewer chrome (release notes live in git) |
| `/docs` (external) | Docs nav link | done | points at the upstream repo |

Redirect-only aliases in the original (no UI on either side):
`auto-combo`, `compression` (bare), `context` (bare), `limits`,
`settings` (bare), `usage` (bare → logs). Sub-route-only groups with no
bare page on either side: `gamification`, `resilience`, `system`, `tools`.
Unlisted but functional in the original with no Rust equivalent: `relay`
(token CRUD), `onboarding` (covered by the providers onboarding wizard +
first-run hint), `media-providers/[kind]`.
