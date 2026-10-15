// Tells an open tab that a new deploy has taken over.
//
// `sw.js` is rebuilt with every deploy, so the browser installs the new one on
// the next page load or update check, and it takes control at once
// (`skipWaiting` + `clients.claim`). A tab that was already open is still
// running the old modules, though: this notices the hand-over
// (`controllerchange`), asks the new worker for its build's file list, and
// offers a reload — once — only if this page's own hashed files are not in it.
// So the first install on a fresh tab says nothing, and neither does the tab
// whose load fetched the new pages and so triggered the install.
//
// The notice is built with `createElement`/`textContent` only, like the rest
// of the app.

export const UPDATE_MESSAGE = "Living Brain was updated.";

/**
 * The hashed files this page runs on: `moduleUrl` (app.js's own
 * `import.meta.url`) and the page's stylesheets, as absolute URLs, keeping
 * only the content-hashed ones under `/static/`. Empty for the unbuilt app.
 */
export function pageAssets(doc, moduleUrl) {
  const urls = [];
  if (moduleUrl) urls.push(String(moduleUrl));
  if (doc && typeof doc.querySelectorAll === "function") {
    for (const link of doc.querySelectorAll('link[rel="stylesheet"]')) {
      if (link && link.href) urls.push(String(link.href));
    }
  }
  return urls.filter((url) => {
    try {
      return new URL(url).pathname.startsWith("/static/");
    } catch {
      return false;
    }
  });
}

/**
 * True when every one of `assets` is in the build `answer` describes
 * (`{ precache: ["./static/…", …] }`, resolved against `origin`). A build
 * with no precache — the unbuilt app — counts as current.
 */
export function isCurrentBuild(answer, assets, origin) {
  const precache = answer && Array.isArray(answer.precache) ? answer.precache : null;
  if (!precache) return false;
  if (precache.length === 0 || assets.length === 0) return true;
  const paths = new Set(
    precache.map((entry) => {
      try {
        return new URL(entry, `${origin}/`).pathname;
      } catch {
        return "";
      }
    }),
  );
  return assets.every((url) => {
    try {
      return paths.has(new URL(url).pathname);
    } catch {
      return false;
    }
  });
}

/**
 * Asks `controller` (the worker now in charge) which build it is, over a
 * MessageChannel. Resolves the answer, or null after `timeoutMs`.
 */
export function askBuild(controller, timeoutMs = 3000) {
  return new Promise((resolve) => {
    if (!controller || typeof controller.postMessage !== "function" || typeof MessageChannel !== "function") {
      resolve(null);
      return;
    }
    const channel = new MessageChannel();
    const timer = setTimeout(() => {
      channel.port1.close();
      resolve(null);
    }, timeoutMs);
    channel.port1.onmessage = (event) => {
      clearTimeout(timer);
      channel.port1.close();
      resolve(event.data || null);
    };
    try {
      controller.postMessage({ type: "lb-build" }, [channel.port2]);
    } catch {
      clearTimeout(timer);
      resolve(null);
    }
  });
}

/**
 * Wires the update check onto a `navigator.serviceWorker`-shaped container.
 *
 * On each hand-over to a new worker, `isCurrent()` (a promise of a boolean)
 * decides whether this tab already runs that build; if not, `onUpdate()` is
 * called — at most once per tab. Also asks the browser to look for a new
 * worker whenever the tab comes back into view (`doc` visibilitychange), so a
 * tab left open for days still notices a deploy.
 */
export function watchForUpdates(container, onUpdate, { doc, isCurrent } = {}) {
  if (!container || typeof container.addEventListener !== "function") return false;
  const current = typeof isCurrent === "function" ? isCurrent : () => Promise.resolve(false);
  let fired = false;
  container.addEventListener("controllerchange", () => {
    if (fired) return;
    Promise.resolve()
      .then(() => current(container.controller))
      .catch(() => false)
      .then((same) => {
        if (same || fired) return;
        fired = true;
        onUpdate();
      });
  });
  if (doc && typeof doc.addEventListener === "function") {
    doc.addEventListener("visibilitychange", () => {
      if (doc.visibilityState !== "visible" || typeof container.getRegistration !== "function") {
        return;
      }
      Promise.resolve(container.getRegistration())
        .then((registration) => registration && registration.update())
        .catch(() => {});
    });
  }
  return true;
}

/** The whole page-side wiring, as `app.js` calls it. */
export function startUpdateWatch(container, doc, moduleUrl, reload) {
  const origin = doc && doc.location ? doc.location.origin : "";
  return watchForUpdates(container, () => showUpdateNotice(doc, reload), {
    doc,
    isCurrent: (controller) =>
      askBuild(controller).then((answer) =>
        isCurrentBuild(answer, pageAssets(doc, moduleUrl), origin),
      ),
  });
}

/** Shows the "updated — reload" notice in `doc`; `reload` runs on the button. */
export function showUpdateNotice(doc, reload) {
  if (!doc || !doc.body || typeof doc.createElement !== "function") return null;
  const existing = doc.getElementById && doc.getElementById("lb-update");
  if (existing) return existing;
  const box = doc.createElement("div");
  box.id = "lb-update";
  box.className = "toast update-notice";
  box.setAttribute("role", "status");
  box.setAttribute("aria-live", "polite");
  box.dataset.tone = "ok";
  const text = doc.createElement("span");
  text.textContent = UPDATE_MESSAGE;
  const button = doc.createElement("button");
  button.type = "button";
  button.className = "btn btn--primary update-notice__reload";
  button.textContent = "Reload";
  button.addEventListener("click", () => reload());
  box.appendChild(text);
  box.appendChild(button);
  doc.body.appendChild(box);
  return box;
}
