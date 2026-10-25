# AGENTS.md

## Project

- Single-crate Rust gateway: package `omniroute-rust`, binary `omniroute`, library crate `omniroute_rust` (edition 2021). OpenAI/Claude/Gemini-compatible endpoints run on axum + tokio.
- Request flow: `src/server/mod.rs` (routes) -> `src/core/chat_core.rs` (resolve, route, compress, execute, translate, stream) -> `src/upstream/executor.rs` (URL/header synthesis) -> `src/translate/` (wire formats). Providers: `src/registry.rs`; config: `src/config.rs`; shared state: `src/state.rs`; combos/rate/circuit: `src/router/`.
- Dashboard assets in `src/server/dashboard_assets/` are embedded with `include_str!` / `include_bytes!`; edit them there and rebuild. There is no separate dashboard build step. `electron/` is an optional wrapper around `target/release/omniroute`.
- The OpenCode Zen free lane (`src/opencode_free.rs`) is fingerprint-sensitive: session-id shape, header set, forced streaming, gate tools, and the default `public` credential are load-bearing. Do not swap in UUIDs or "simplify" headers. Read `tests/integration.rs::opencode_free_lane_enforces_the_anonymous_contract_and_learns_denials` before changing this lane.
- A free-lane `429 FreeUsageLimitError` is quota exhaustion, not a fingerprint failure; `403 FreeTierError` is the request-shape rejection. Do not "fix" the fingerprint for a quota 429.

## Build and verify

- `cargo build --release` -> `target/release/omniroute`.
- `cargo test` runs unit + all integration/probe suites. Focused: `cargo test --lib`; `cargo test --test integration`; one integration test: `cargo test --test integration <name>`.
- Integration tests under `tests/` use a local mock upstream, ephemeral ports, and `tempfile` dirs; they need no real provider keys. New tests should pass explicit `Config`/`AppState` rather than mutate process env; tests run in parallel and one dashboard test sets `OMNIROUTE_DATA_DIR` globally.
- The README's Testing block is stale (test counts and "0 warnings"); trust actual cargo output.
- `cargo clippy --all-targets` exits 0 but prints pre-existing warnings; `cargo clippy --all-targets -- -D warnings` fails. Do not mass-fix unrelated warnings, and do not add new ones.
- Do NOT run `cargo fmt` / `cargo fmt --all`: the tree is not rustfmt-clean and repo-wide formatting creates a huge unrelated diff. Match surrounding style and format only touched code by hand.
- There is no CI or pre-commit hook; local commands are the gate.

## Run and configure

- Safe local run: `OMNIROUTE_DATA_DIR=$(mktemp -d) ./target/release/omniroute serve --port 20129`.
- Defaults: `127.0.0.1:20128`, data dir `~/.omniroute-rust`. Data dir override: `OMNIROUTE_DATA_DIR`, then `DATA_DIR` (first wins). Never point a local or benchmark run at the live data dir.
- `omniroute stop` reads `$DATA_DIR/omniroute.pid`; set the same data dir as the process you intend to stop.
- Config layering (first wins): `$DATA_DIR/provider-credentials.json`, `$DATA_DIR/omniroute.toml`, `.env` in `$DATA_DIR` -> `~/.omniroute-rust` -> cwd, then process environment. Do not create `.env` in the repo.
- `/v1/*` requires a Bearer key when `OMNIROUTE_API_KEY` or a dashboard-issued key is configured; management endpoints use a dashboard `sess_*` session or an admin key. The first dashboard password is `CHANGEME` until changed (`OMNIROUTE_ADMIN_PASSWORD` can pin it). Change it; never commit or echo it.

## Deploy

- `deploy/README.md` documents the service; unit template `deploy/omniroute-rust.service`, service name `omniroute-rust`.
- The repo template is a WSL user-service example; do not assume it matches the live host's unit or paths. Inspect the live unit before changing it.
- Production host/IP, SSH port, and credentials are out-of-band. Never write them into the repo, commit messages, tests, examples, or this file. Build release, install the binary, restart the service, then check `/healthz` and `/dashboard`.

## Git and privacy

- Check `git config user.name` / `user.email` before committing and use the repository owner's configured identity. Never commit with a machine-local identity such as `user@local` or `user@hostname`.
- Conventional Commits in English, imperative: `type(scope): summary` (for example `fix(opencode): ...`). Put the reason in the body. One logical change per commit; run `cargo test` first. Docs may be bilingual (`README_zh.md`, `docs/zh/`), but commit messages stay English.
- Version lives in `Cargo.toml`; release tags are `vX.Y.Z`. Do not bump the version or add/move tags unless asked.
- The repository is public. Treat every commit, tag, branch, PR, issue, test fixture, and example as public.
- Never commit or paste production IPs/hostnames, SSH ports, deployment URLs, tokens, cookies, API keys, provider credentials, dashboard sessions/password hashes, `$DATA_DIR` contents, or request logs containing secrets. Use `<gateway-host>`, `<ssh-port>`, `<api-key>` placeholders.
- `.gitignore` covers `.env`, `provider-credentials.json`, `dashboard-auth.json`, `target/`, `.agent/`, and `.tmp-gh/`, but it is not a guarantee. Inspect `git diff --cached` before every commit.
- Keep one-off tokens and history bundles outside the repo with mode `600`; delete or revoke them after use.
- If sensitive data was committed: assume it is compromised, rotate it, then delete/recreate the public repo (or ask GitHub Support). A force-push or amend does not purge old GitHub objects; old SHAs remain fetchable, and recreation loses stars. Get owner approval first.
