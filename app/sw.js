/* The app-shell cache.
 *
 * Built, not hand-maintained: `tools/stamp-assets.mjs` replaces `VERSION` with
 * a hash of the deployed build and `PRECACHE` with the pages plus every
 * content-hashed file they load (`static/assets/app.<hash>.js` and so on).
 * Each deploy therefore ships a byte-different worker, which is what makes the
 * browser install it; nothing here is ever bumped by hand. Served unbuilt
 * (straight from `app/`), it precaches nothing and only falls back to what it
 * has seen.
 *
 * - Pages: network first, so a deploy shows on the next load; the cached copy
 *   is only for when the network fails.
 * - `static/*`: cache first. A hashed name never changes content, so a hit is
 *   always right, and a new deploy simply asks for new names.
 * - Anything else of ours: network first, cache as the offline fallback.
 * - `/v1/*` is never cached and never intercepted: the session is an HttpOnly
 *   cookie and the app's data must never come from a stale cache.
 *
 * A new worker takes over at once (`skipWaiting` + `clients.claim`) and drops
 * every older `lb-app-*` cache; open tabs see `controllerchange` and offer a
 * reload (`assets/update.js`). A broken service worker is worse than none, so
 * every step is defensive — a failed install leaves the app on the network.
 */

const VERSION = "dev";
const PRECACHE = [];

const CACHE_PREFIX = "lb-app-";
const CACHE = CACHE_PREFIX + VERSION;

/** One cache key per page: `/wiki.html?view=x`, `/wiki.html` and `/wiki` are the same page. */
function pageKey(url) {
  const path = url.pathname.replace(/\/index(?:\.html)?$/, "/").replace(/\.html$/, "");
  return new URL(path, url.origin).href;
}

function isPage(request, url) {
  return (
    request.mode === "navigate" ||
    url.pathname.endsWith("/") ||
    url.pathname.endsWith(".html")
  );
}

/**
 * A response safe to store: a redirected response (`/wiki.html` → `/wiki`)
 * cannot answer a navigation, so it is stored as a plain copy.
 */
function storable(response) {
  if (!response || !response.ok || response.type === "opaque") return null;
  if (!response.redirected) return response.clone();
  return response
    .clone()
    .blob()
    .then(
      (body) =>
        new Response(body, {
          status: response.status,
          statusText: response.statusText,
          headers: response.headers,
        }),
    );
}

function put(key, response) {
  const copy = storable(response);
  if (!copy) return Promise.resolve();
  return Promise.resolve(copy)
    .then((body) => caches.open(CACHE).then((cache) => cache.put(key, body)))
    .catch(() => {});
}

self.addEventListener("install", (event) => {
  event.waitUntil(
    Promise.all(
      PRECACHE.map((path) => {
        const url = new URL(path, self.location.href);
        const key = url.pathname.startsWith("/static/") ? url.href : pageKey(url);
        // `no-cache`: never precache what the HTTP cache still holds from an
        // older deploy.
        return fetch(url.href, { cache: "no-cache", credentials: "same-origin" })
          .then((response) => put(key, response))
          .catch(() => {});
      }),
    )
      .then(() => self.skipWaiting())
      .catch(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) =>
        Promise.all(
          names
            .filter((n) => n.startsWith(CACHE_PREFIX) && n !== CACHE)
            .map((n) => caches.delete(n)),
        ),
      )
      .then(() => self.clients.claim())
      .catch(() => self.clients.claim()),
  );
});

// An open tab asks, after a hand-over, whether this build is the one it is
// already running (`assets/update.js`): it compares its own hashed files with
// this precache list, so a tab that loaded the new pages is not told to
// reload into them again.
self.addEventListener("message", (event) => {
  const port = event.ports && event.ports[0];
  if (!port || !event.data || event.data.type !== "lb-build") return;
  port.postMessage({ type: "lb-build", version: VERSION, precache: PRECACHE });
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  // The API is never cached and never intercepted.
  if (url.pathname.startsWith("/v1/")) return;

  if (url.pathname.startsWith("/static/")) {
    event.respondWith(
      caches
        .match(request)
        .catch(() => null)
        .then(
          (hit) =>
            hit ||
            fetch(request).then((response) => {
              event.waitUntil(put(request, response));
              return response;
            }),
        ),
    );
    return;
  }

  const key = isPage(request, url) ? pageKey(url) : request;
  event.respondWith(
    fetch(request)
      .then((response) => {
        event.waitUntil(put(key, response));
        return response;
      })
      .catch(() =>
        caches
          .match(key)
          .catch(() => null)
          .then((hit) => hit || Response.error()),
      ),
  );
});
