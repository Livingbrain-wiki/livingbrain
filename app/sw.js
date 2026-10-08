/* The app-shell cache.
 *
 * Deliberately minimal: it precaches the three pages and the two asset files,
 * serves same-origin GETs from the cache when the network fails, and does
 * nothing at all to anything it did not put there itself. A broken service
 * worker is worse than none, so every step is defensive — a failed install
 * leaves the app working from the network alone.
 *
 * Nothing from `/v1/*` is ever cached: the session is an HttpOnly cookie and
 * the app's data must never be served from a stale cache.
 */

const CACHE = "lb-app-v1";

const SHELL = [
  "./",
  "./index.html",
  "./settings.html",
  "./wiki.html",
  "./assets/app.css",
  "./assets/app.js",
  "./assets/markdown.js",
  "./assets/settings.js",
  "./manifest.webmanifest",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .then(() => self.skipWaiting())
      .catch(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) =>
        Promise.all(names.filter((n) => n !== CACHE).map((n) => caches.delete(n))),
      )
      .then(() => self.clients.claim())
      .catch(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  // The API is never cached and never intercepted.
  if (url.pathname.startsWith("/v1/")) return;

  event.respondWith(
    fetch(request)
      .then((response) => {
        if (response && response.ok) {
          const copy = response.clone();
          caches.open(CACHE).then((cache) => cache.put(request, copy)).catch(() => {});
        }
        return response;
      })
      .catch(() =>
        caches.match(request).then((hit) => hit || Response.error()),
      ),
  );
});