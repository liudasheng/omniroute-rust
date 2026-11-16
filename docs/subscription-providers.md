# Codex and Antigravity subscription accounts

The Rust gateway supports existing OAuth credentials for `codex` and
`antigravity`. This is independent of OpenAI API-key billing and Google Gemini
API keys. Browser/device OAuth login and npm SQLite migration are not implemented
by this change.

## Account storage and management

Accounts live in `$OMNIROUTE_DATA_DIR/provider-connections.json` (default
`~/.omniroute-rust`), never in the npm gateway's `~/.omniroute` directory.
The existing authenticated `/v1/provider-connections` create/update APIs accept:

- `access_token` / `accessToken`
- `refresh_token` / `refreshToken`
- `id_token` / `idToken`
- `expires_at` / `expiresAt`: absolute Unix epoch **milliseconds**
- `project_id` / `projectId`: Antigravity Cloud Code project
- `provider_specific_data` / `providerSpecificData`: optional provider metadata
  (`workspaceId` for Codex, `projectId` for Antigravity)

The Providers account form also accepts existing tokens, expiry and project or
workspace ID. It does **not** perform a new browser sign-in. Management list
responses expose token presence and expiry, not token contents. The connection
store is plaintext (like its existing API keys); Unix writes use mode 0600.
On Windows protect the data directory with a user-only ACL, and do not put it in
a shared/synced directory. Never commit credentials or put tokens in shell history.

A model not present in the seed catalog can be explicitly listed on a connection.
Subscription providers do not expose the standard API-key `/v1/models` route;
background model discovery therefore preserves their configured catalog.

## Refresh and execution

- Refresh happens before dispatch if expiry is within 60 seconds or access token
  is absent. A 401 triggers one refresh/retry. No endless refresh loop.
- Concurrent refreshes are serialized **per account**, with a fresh store read
  after locking. Rotated refresh tokens are persisted before use.
- Codex refresh intentionally omits `scope` to avoid re-scoping sibling token
  families. Failed token endpoint bodies are not exposed in errors or logs.
- Do not run npm and Rust gateways against the same Codex refresh token at the
  same time: locks are local to one process; rotation may invalidate the other's
  copy. Stop npm before a real migration test.
- Codex sends Responses requests to the ChatGPT subscription backend, always
  streaming with `store:false`; workspace binding uses `chatgpt-account-id`.
- Antigravity wraps native Gemini requests in a Cloud Code envelope, including
  Claude-branded models. An absent project ID is discovered with
  `loadCodeAssist` and persisted when assigned. The gateway does not fabricate
  project IDs or automatically create a Google Cloud project; accounts requiring
  a manual project return 422 with instructions.
- Incoming Chat Completions, Responses and Claude requests reuse the existing
  translators. SSE stays incremental; non-stream clients receive folded JSON.
- Auth/quota failures try eligible sibling accounts before the next Combo target.
- Optional per-account `proxy_url` applies to refresh and generation alike.

Public installed-client OAuth defaults are ported from npm OmniRoute's
`publicCreds.ts`. These are public client identifiers, not account credentials.
Deployments may override `CODEX_OAUTH_CLIENT_ID`,
`ANTIGRAVITY_OAUTH_CLIENT_ID`, `ANTIGRAVITY_OAUTH_CLIENT_SECRET` and client
version headers (`CODEX_CLIENT_VERSION`, `ANTIGRAVITY_CLIENT_VERSION`).
Antigravity metadata `clientProfile: "cli"` selects the Go CLI profile instead of
IDE (`ANTIGRAVITY_CLI_VERSION` overrides its version). Metadata may specify
`oauthClientId` / `oauthClientSecret` for an
account issued by a custom OAuth client. `tokenUrl` is trusted account metadata
for local tests/custom deployments; never accept untrusted account configuration.

This is not complete feature parity with npm OmniRoute: native Codex WebSocket /
app-server transports, automatic version discovery, provider quota dashboards,
new OAuth sign-in and every npm-specific routing strategy are outside this change.

## Combos

Existing Rust Combo management, nesting and strategies remain in use. For
example, an `omniroute.toml` combo can route across the two subscription providers:

```toml
[[combos]]
name = "subscription-coding"
strategy = "priority"
providers = ["codex/gpt-5.4", "antigravity/gemini-3-flash"]
```

Send `model: "subscription-coding"` to `/v1/chat/completions`, or create the
same ordered targets in Combo Studio. This does not require another combo engine.

## Windows 11 build and isolated test run

Build prerequisites for the existing fingerprint-sensitive BoringSSL transport:
Rust MSVC toolchain, Visual Studio C++ build tools + Windows SDK, CMake, NASM on
PATH and libclang (`LIBCLANG_PATH` pointing to the directory containing
`libclang.dll`). Do not replace the OpenCode transport just to bypass build tools.

```powershell
cargo test -j 1 -- --test-threads=1
cargo build -j 1 --release --bin omniroute
$env:OMNIROUTE_DATA_DIR = Join-Path $env:TEMP 'omniroute-rust-trial'
.\target\release\omniroute.exe serve --port 20129
```

Open `http://127.0.0.1:20129/dashboard`. Use Ctrl+C to stop a foreground trial.
The CLI also supports Windows process checks and `stop`. If `HOME` is not set,
the default data directory resolves under `USERPROFILE`.

Tests in `tests/oauth_providers.rs` use local mocks and synthetic credentials;
no real provider keys, npm database access or live OAuth refresh is required.
