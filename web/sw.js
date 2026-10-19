// Service worker: lets the app open with no network (the document itself
// comes from IndexedDB). Strategy for the app shell: network first, so code
// updates show up immediately when online; fall back to the cache offline.

const CACHE = "collab-v1";
const SHELL = [
  "./",
  "index.html",
  "style.css",
  "manifest.webmanifest",
  "icon.svg",
  "dist/main.js",
  "dist/crdt.js",
  "dist/editor.js",
  "dist/protocol.js",
  "dist/storage.js",
  "dist/sync.js",
  "dist/undo.js",
];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET" || url.origin !== location.origin) return;
  if (url.pathname === "/ws" || url.pathname.startsWith("/export/") || url.pathname === "/health") return;

  e.respondWith(
    fetch(e.request)
      .then((res) => {
        if (res.ok) {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put(e.request, copy));
        }
        return res;
      })
      .catch(async () => {
        // "/?doc=abc" and "/" are the same page: ignore the query string.
        const hit = await caches.match(e.request, { ignoreSearch: true });
        return hit ?? (await caches.match("index.html")) ?? Response.error();
      }),
  );
});
