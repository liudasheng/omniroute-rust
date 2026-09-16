/**
 * omniroute-rust Electron Desktop Shell — Main Process
 *
 * Parity with the original OmniRoute desktop app (`electron/main.js`):
 * spawns the gateway server, waits for /healthz readiness (#1), creates the
 * BrowserWindow loading the embedded dashboard, and manages the system tray.
 *
 * Divergence (documented in docs/PARITY.md): no auto-updater, no
 * encrypted-credential inspection, no remote-server mode — the Rust gateway
 * is started from the local binary produced by `cargo build --release`.
 */

const { app, BrowserWindow, Tray, Menu, nativeImage, shell } = require("electron");
const path = require("path");
const { spawn } = require("child_process");
const http = require("http");

const PORT = Number(process.env.OMNIROUTE_PORT || process.env.PORT || 20128);
const HOST = "127.0.0.1";
const DASHBOARD_URL = `http://${HOST}:${PORT}/dashboard`;
const HEALTH_URL = `http://${HOST}:${PORT}/healthz`;

let server = null; // ChildProcess
let restarting = false;
let ready = false;
let win = null;
let tray = null;

const ROOT = path.join(__dirname, "..");
const BINARY_CANDIDATES = [
  process.env.OMNIROUTE_BINARY,
  path.join(ROOT, "target", "release", "omniroute"),
  path.join(ROOT, "target", "debug", "omniroute"),
].filter(Boolean);

function firstExistingBinary() {
  const fs = require("fs");
  for (const p of BINARY_CANDIDATES) {
    try {
      if (fs.existsSync(p)) return p;
    } catch {}
  }
  return null;
}

function httpOk(url) {
  return new Promise((resolve) => {
    const req = http.get(url, (res) => {
      res.resume();
      resolve(res.statusCode === 200);
    });
    req.on("error", () => resolve(false));
    req.setTimeout(1500, () => {
      req.destroy();
      resolve(false);
    });
  });
}

async function waitForServer(timeoutMs = 60000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (await httpOk(HEALTH_URL)) return true;
    await new Promise((r) => setTimeout(r, 300));
  }
  return false;
}

async function startServer() {
  // If a gateway is already listening on the port, reuse it.
  if (await httpOk(HEALTH_URL)) {
    console.log("[Electron] Gateway already running on :" + PORT);
    return null;
  }
  const binary = firstExistingBinary();
  if (!binary) {
    console.error("[Electron] omniroute binary not found — run `cargo build --release` first");
    return null;
  }
  server = spawn(binary, ["serve", "--port", String(PORT), "--no-open"], {
    cwd: ROOT,
    env: { ...process.env, PORT: String(PORT) },
    stdio: "pipe",
  });
  server.stdout.on("data", (d) => process.stdout.write("[omniroute] " + d));
  server.stderr.on("data", (d) => process.stderr.write("[omniroute] " + d));
  server.on("exit", (code) => {
    console.log(`[Electron] gateway exited (${code})`);
    ready = false;
    // crash restart (parity: ServerSupervisor with max-restarts)
    if (!app.isQuitting && !restarting) restartServer();
  });
  return server;
}

async function stopServer() {
  restarting = true;
  if (server) {
    server.kill("SIGTERM");
    await new Promise((r) => setTimeout(r, 1500));
    if (server && server.exitCode === null) server.kill("SIGKILL");
    server = null;
  }
  restarting = false;
}

async function restartServer() {
  await stopServer();
  await startServer();
  ready = await waitForServer();
}

function createWindow() {
  win = new BrowserWindow({
    width: 1280,
    height: 840,
    backgroundColor: "#0b0f19",
    autoHideMenuBar: true,
    icon: path.join(__dirname, "assets", "icon256.png"),
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  win.loadURL(DASHBOARD_URL);
  // keep app running in tray on close (parity: tray background mode)
  win.on("close", (e) => {
    if (!app.isQuitting) {
      e.preventDefault();
      win.hide();
    }
  });
  win.webContents.setWindowOpenHandler(({ url }) => {
    shell.openExternal(url);
    return { action: "deny" };
  });
}

function createTray() {
  const icon = nativeImage.createFromPath(path.join(__dirname, "assets", "icon32.png"));
  tray = new Tray(icon);
  tray.setToolTip("omniroute-rust");
  const menu = Menu.buildFromTemplate([
    { label: "Open dashboard", click: () => (win ? (win.show(), win.focus()) : createWindow()) },
    { label: `Server: ${HOST}:${PORT}`, enabled: false },
    { type: "separator" },
    {
      label: "Restart server",
      click: async () => {
        await restartServer();
        ready = await waitForServer();
        if (win) win.webContents.reload();
      },
    },
    { type: "separator" },
    {
      label: "Quit",
      click: () => {
        app.isQuitting = true;
        app.quit();
      },
    },
  ]);
  tray.setContextMenu(menu);
  tray.on("click", () => (win ? (win.isVisible() ? win.hide() : (win.show(), win.focus())) : createWindow()));
}

const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  app.on("second-instance", () => {
    if (win) {
      win.show();
      win.focus();
    }
  });

  app.whenReady().then(async () => {
    await startServer();
    ready = await waitForServer();
    createWindow();
    createTray();
    console.log(`[Electron] dashboard ready=${ready} → ${DASHBOARD_URL}`);
  });

  app.on("window-all-closed", (e) => {
    // keep running in the tray (parity: tray background mode)
  });

  app.on("before-quit", async () => {
    app.isQuitting = true;
    await stopServer();
  });
}
