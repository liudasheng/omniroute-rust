/* omniroute-rust dashboard service worker — network-first with cached shell */
const CACHE = "omniroute-dashboard-v3";
const SHELL = ["/dashboard", "/dashboard/app.css", "/dashboard/app.js", "/dashboard/icon.svg"];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => caches.open(CACHE).then((c2) => c2.addAll(SHELL))).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys().then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))).then(() => self.clients.claim())
  );
});

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET" || url.pathname.startsWith("/v1/")) return; // never cache API
  e.respondWith(
    fetch(e.request)
      .then((r) => {
        const copy = r.clone();
        caches.open(CACHE).then((c) => c.put(e.request, copy));
        return r;
      })
      .catch(() => caches.match(e.request).then((r) => r || caches.match("/dashboard")))
  );
});
