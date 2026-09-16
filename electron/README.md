# omniroute-rust Desktop (Electron shell)

Desktop shell for the Rust gateway — parity with the original OmniRoute
desktop app (`electron/main.js`): spawns the gateway, waits for `/healthz`,
opens the embedded dashboard window, and lives in the system tray.

## Run (dev)

```bash
# 1) build the gateway first
cargo build --release
# 2) start the desktop shell
cd electron
npm install
npm start            # add -- --no-sandbox on some Linux setups
```

The shell:

- reuses a gateway already listening on `:20128` (or `OMNIROUTE_PORT`)
- spawns `../target/release/omniroute serve --no-open` otherwise
- waits for `/healthz` readiness (up to 60s) before loading the window
- restarts the gateway if it crashes (parity: ServerSupervisor)
- close hides to tray; tray menu → open/restart/quit

## Package

```bash
cd electron
npm run build   # electron-builder; bundles the release binary via extraResources
```

Divergences from the original (see docs/PARITY.md §8): no auto-updater, no
remote-server login mode, no encrypted-credential inspection.
