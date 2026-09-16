/* omniroute-rust desktop preload — minimal IPC bridge (parity: electron/preload.js) */
const { contextBridge } = require("electron");

contextBridge.exposeInMainWorld("omniroute", {
  desktop: true,
  version: process.versions.electron,
});
