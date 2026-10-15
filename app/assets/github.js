// The GitHub connection in Settings → Integrations, and the GitHub source
// filter's helpers — the pure half, next to `members.js` and `tokens.js`.
// Requests are described here and sent by `mountGithub`, which the settings
// page calls with its own `api`; every row is painted with elements and text
// nodes only, never markup from a string, as everywhere in `app/`.
//
// The routes are in docs/github.md ("Routes and the drain", "The shared app").
// The default way in is the shared "Living Brain" GitHub App: one button that
// follows an install link to github.com (navigation, never a form). "Use your
// own GitHub App" is the advanced manifest flow, the one form posted to GitHub.
// Nothing here starts a backfill: the server's cron does that, and there is no
// button for it.

/** Every GitHub settings route hangs off this path. */
export const GITHUB_PATH = "/v1/github";

/** Where GitHub sends a person, and the only origin a link from the server may name. */
export const GITHUB_ORIGIN = "https://github.com/";

/** A GitHub organization login: alphanumerics and single hyphens, 1..=39. */
const ORG_LOGIN = /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?$/;

/**
 * The owner the app is created under, from the form: the person's own
 * account, or an organization login typed in. `{ ok, body }` is the manifest
 * request's body; `{ ok: false, error }` says what to fix first.
 */
export function ownerChoice(kind, org = "") {
  if (kind !== "org") return { ok: true, body: { owner: "user" } };
  const login = String(org || "").trim();
  if (!ORG_LOGIN.test(login)) {
    return { ok: false, error: "Type the organization's login, as it appears in its GitHub URL." };
  }
  return { ok: true, body: { owner: "org", org: login } };
}

/** `POST /v1/github/app/manifest` for an owner choice. */
export function manifestRequest(body) {
  return { method: "POST", path: `${GITHUB_PATH}/app/manifest`, body };
}

/** `POST /v1/github/shared/install`: the shared app's install link. */
export function sharedInstallRequest() {
  return { method: "POST", path: `${GITHUB_PATH}/shared/install` };
}

/** `POST /v1/github/shared/verify`: another OAuth hop for an unclaimed installation. */
export function sharedVerifyRequest(installationId) {
  return { method: "POST", path: `${GITHUB_PATH}/shared/verify`, body: { installation_id: installationId } };
}

/**
 * The installation a `reason=installer-unknown` address names, as a number,
 * or null: only digits are ever read from the address.
 */
export function retryInstallation(search = "") {
  const params = new URLSearchParams(String(search || "").replace(/^\?/, ""));
  if (params.get("github") !== "error" || params.get("reason") !== "installer-unknown") return null;
  const raw = params.get("installation") || "";
  if (!/^[0-9]{1,15}$/.test(raw)) return null;
  const id = Number(raw);
  return id > 0 ? id : null;
}

/** `POST /v1/github/app/install`: a fresh install link. */
export function installRequest() {
  return { method: "POST", path: `${GITHUB_PATH}/app/install` };
}

/** `PATCH /v1/github/repos/{id}`: the only repository write the page makes. */
export function repoRequest(repoId, patch) {
  return { method: "PATCH", path: `${GITHUB_PATH}/repos/${encodeURIComponent(String(repoId))}`, body: patch };
}

/** `DELETE /v1/github/app`: forgets the app here; GitHub keeps it. */
export function removeAppRequest() {
  return { method: "DELETE", path: `${GITHUB_PATH}/app` };
}

/** Whether a URL from the server is a GitHub page the browser may follow. */
export function isGithubLink(url) {
  return typeof url === "string" && url.startsWith(GITHUB_ORIGIN) && !/[\s"'<>]/.test(url);
}

/**
 * The manifest form: a POST to GitHub's action with one hidden field,
 * `manifest`, carrying the JSON the server minted. Built, never parsed, so the
 * manifest's text is only ever a field's value. Refuses any action that is
 * not on GitHub.
 */
export function buildManifestForm(doc, { action, manifestJson }) {
  if (!isGithubLink(action) || typeof manifestJson !== "string" || !manifestJson) {
    throw new Error("GitHub did not return a manifest form Living Brain can send.");
  }
  const form = doc.createElement("form");
  form.setAttribute("method", "POST");
  form.setAttribute("action", action);
  form.setAttribute("class", "github-manifest");
  const field = doc.createElement("input");
  field.setAttribute("type", "hidden");
  field.setAttribute("name", "manifest");
  field.value = manifestJson;
  form.appendChild(field);
  return form;
}

/**
 * The banner for `?github=<outcome>&reason=<reason>` on `settings.html#github`:
 * `{ tone, text }`, or null when the address names no outcome. Outcomes and
 * reasons are the ones docs/github.md lists; an unknown reason still says the
 * setup did not finish, and never echoes the raw value.
 */
export function outcomeBanner(search = "") {
  const params = new URLSearchParams(String(search || "").replace(/^\?/, ""));
  const outcome = params.get("github");
  if (!outcome) return null;
  const reason = params.get("reason") || "";
  if (outcome === "installed") {
    if (reason === "repos") {
      return {
        tone: "warn",
        text: "GitHub accepted the install, but Living Brain could not read its repositories yet. Reload this page in a moment. If it persists, use Add another account.",
      };
    }
    if (reason === "suspended") {
      return {
        tone: "warn",
        text: "The installation is suspended on GitHub, so nothing syncs until it is resumed there.",
      };
    }
    return {
      tone: "good",
      text: "GitHub is connected. Included repositories are backfilled on the server's next cron tick; progress shows below.",
    };
  }
  if (outcome === "requested") {
    return {
      tone: "warn",
      text: "Asked an owner of the organization to approve the app. Once they approve it, come back here and use Add another account.",
    };
  }
  if (outcome === "created") {
    return {
      tone: "warn",
      text: "The GitHub app was saved, but its install link could not be started. Use Add another account to install it.",
    };
  }
  if (outcome === "error") {
    const known = Object.prototype.hasOwnProperty.call(ERROR_TEXT, reason);
    return { tone: "bad", text: known ? ERROR_TEXT[reason] : "GitHub setup did not finish. Try again." };
  }
  return null;
}

/** What each `outcome=error` reason means to the person who sees it. */
export const ERROR_TEXT = Object.freeze({
  "bad-request": "GitHub sent back a request Living Brain could not read. Start again from here.",
  "signed-out": "Sign in to finish connecting GitHub.",
  "not-configured": "GitHub is not set up on this server yet.",
  state: "That GitHub step expired or was already used. Start it again.",
  conversion: "GitHub did not finish creating the app. Try again.",
  "app-exists": "This workspace already has a GitHub app.",
  "app-id-taken": "That GitHub app is already connected to another workspace.",
  forbidden: "Only a workspace owner or admin can connect GitHub.",
  store: "Living Brain could not save the GitHub connection. Try again.",
  "no-app": "Create the GitHub app first.",
  github: "GitHub did not answer as expected. Try again in a moment.",
  "not-confirmed": "GitHub does not show that installation for this app, so it was not connected.",
  "bound-elsewhere": "That installation is already connected to another workspace.",
  refused: "Living Brain refused that installation.",
  unclaimed:
    "That installation was made on GitHub directly, so it is not connected. Use Install Living Brain on GitHub here, as an owner or admin.",
  "not-verified":
    "GitHub did not confirm that your GitHub account can reach that installation, so it was not connected. Sign in to GitHub as the person who installed it and try again.",
  "not-installer":
    "Only the GitHub user who installed Living Brain on that account can connect it. Ask them to finish here, or install it again yourself.",
  "installer-unknown":
    "GitHub has not yet told Living Brain who installed the app. Wait a few seconds, then choose Finish connecting.",
});

/** The scope as a person reads it: `shared`, or `personal` for `user:<id>`. */
export function scopeLabel(scope) {
  if (scope === "shared") return "Shared";
  if (typeof scope === "string" && scope.startsWith("user:")) return "Personal";
  return String(scope || "");
}

/** A unix-seconds instant as a short local date and time, or "" when absent. */
export function when(seconds) {
  if (typeof seconds !== "number" || !Number.isFinite(seconds) || seconds <= 0) return "";
  return new Date(seconds * 1000).toLocaleString();
}

/** The backfill, in a sentence: its phase, what it has read, and any wait. */
export function backfillText(backfill, now = Date.now() / 1000) {
  const state = (backfill && backfill.state) || "pending";
  const items = Number(backfill && backfill.items) || 0;
  const batches = Number(backfill && backfill.batches) || 0;
  const waiting = backfill && backfill.waiting_until;
  if (typeof waiting === "number" && waiting > now) {
    return `Waiting for GitHub until ${when(waiting)} (${items} items so far)`;
  }
  if (state === "done") return `Backfill done: ${items} items in ${batches} batches`;
  if (state === "running") return `Backfilling: ${items} items in ${batches} batches`;
  return "Pending: the next cron tick starts it";
}

/** One repository row, as the table paints it. */
export function repoView(repo, now = Date.now() / 1000) {
  const error = repo.last_error ? String(repo.last_error) : "";
  return {
    repoId: repo.repo_id,
    name: String(repo.full_name || ""),
    project: String(repo.project || ""),
    scope: scopeLabel(repo.scope),
    included: Boolean(repo.included),
    backfill: backfillText(repo.backfill, now),
    lastWebhook: when(repo.last_webhook_at),
    error,
    errorAt: when(repo.last_error_at),
  };
}

/** The installation as a line: the account, its status, and who bound it. */
export function installationView(entry) {
  const account = (entry && entry.account) || {};
  const status = String((entry && entry.status) || "");
  const kind = entry && entry.app_kind === "workspace" ? "workspace" : entry && entry.app_kind === "shared" ? "shared" : "";
  const via = kind === "workspace" ? " · your own app" : "";
  return {
    id: entry && entry.installation_id,
    line: `${account.login || "(unknown account)"} · ${account.type || "account"} · ${status}${via}`,
    status,
    kind,
    manageUrl: isGithubLink(entry && entry.manage_url) ? entry.manage_url : "",
  };
}

/**
 * Everything the tab shows, from `GET /v1/github`: which block is up, and the
 * rows. `canManage` decides whether the write controls are live at all.
 */
export function githubView(data, now = Date.now() / 1000) {
  const payload = data && typeof data === "object" ? data : {};
  const app = payload.app || null;
  const queue = payload.queue || {};
  const shared = payload.shared && typeof payload.shared === "object" ? payload.shared : {};
  const installations = (Array.isArray(payload.installations) ? payload.installations : []).map(installationView);
  const repos = (Array.isArray(payload.repos) ? payload.repos : []).map((repo) => repoView(repo, now));
  return {
    configured: payload.configured !== false,
    canManage: Boolean(payload.can_manage),
    // The shared app: on when the deployment's secrets are set.
    shared: { configured: shared.configured === true, slug: String(shared.slug || "") },
    // "Use your own GitHub App": on when the server can seal an app's keys.
    advanced: payload.advanced && typeof payload.advanced === "object"
      ? payload.advanced.configured === true
      : payload.configured !== false,
    // The own app's block: "setup" (no own app yet) or "connected".
    state: app ? "connected" : "setup",
    // Anything connected at all, by either app: the accounts and repositories.
    hasConnections: Boolean(app) || installations.length > 0 || repos.length > 0,
    app: app
      ? {
          slug: String(app.slug || ""),
          appId: app.app_id,
          htmlUrl: isGithubLink(app.html_url) ? app.html_url : "",
          installUrl: isGithubLink(app.install_url) ? app.install_url : "",
        }
      : null,
    installations,
    repos,
    queued: Number(queue.queued) || 0,
    dead: Number(queue.dead) || 0,
  };
}

/** The message the page shows for a failed GitHub request. */
export function githubErrorMessage(error, what) {
  const status = error && error.status;
  if (status === 403) return `Only a workspace owner or admin can ${what}.`;
  if (status === 409) return `Could not ${what}: ${(error.message || "it conflicts with the current state").replace(/\.$/, "")}.`;
  if (status === 404) return `Could not ${what}: it is no longer here. Reload the page.`;
  return `Could not ${what}: ${(error && error.message) || "the request failed"}.`;
}

/** The text of one connection-state summary line for the app. */
export function appSummaryLine(view) {
  if (!view.app) return "";
  return `${view.app.slug || "GitHub app"} (app ${view.app.appId ?? "?"})`;
}

/**
 * Mounts the GitHub tab: reads `GET /v1/github`, paints the block that fits,
 * and wires the buttons. `deps` carries the page's own `api`, `ApiError`,
 * `confirm` (the app's confirmation dialog), `notify(element, text, tone)`,
 * the `document`, and the `location` (for the banner and the redirects).
 * Resolves once the first paint is done.
 */
export async function mountGithub(deps) {
  const doc = deps.document;
  const q = (sel) => doc.querySelector(sel);
  const { api, confirm, notify } = deps;
  const status = q("#github-status");
  const banner = q("#github-banner");
  const setup = q("#github-setup");
  const connected = q("#github-connected");
  const readonly = q("#github-readonly");
  const owner = q("#github-owner-org");
  const orgField = q("#github-org");
  const create = q("#github-create");
  const add = q("#github-add");
  const remove = q("#github-remove");
  const repoRows = q("#github-repos");
  const installRows = q("#github-installs");
  const appLine = q("#github-app");
  const queueLine = q("#github-queue");
  const removed = q("#github-removed");
  const sharedBlock = q("#github-shared");
  const sharedOff = q("#github-shared-off");
  const sharedInstall = q("#github-shared-install");
  const advanced = q("#github-advanced");
  const own = q("#github-own");
  const retry = q("#github-retry");

  const outcome = outcomeBanner(deps.location && deps.location.search);
  if (outcome && banner) {
    notify(banner, outcome.text, outcome.tone);
    // The outcome is shown once: a reload should not keep the old banner.
    try {
      if (deps.history) deps.history.replaceState(null, "", `${deps.location.pathname}#github`);
    } catch {
      /* keeping the query in the address is harmless */
    }
  }

  // `reason=installer-unknown&installation=<n>`: the OAuth hop can be retried.
  const retryId = retryInstallation(deps.location && deps.location.search);

  const paint = (view) => {
    if (!view.configured) {
      notify(status, "GitHub isn't configured on this deployment.", "warn");
      setHidden([setup, connected, readonly, sharedBlock, sharedOff, advanced, own, retry], true);
      return;
    }
    notify(status, "", "");
    // The default: the shared app's install button, or why it is missing.
    setHidden([sharedBlock], !view.shared.configured);
    setHidden([sharedOff], view.shared.configured);
    if (sharedOff && !view.shared.configured) {
      sharedOff.textContent = view.advanced
        ? "GitHub isn't configured on this deployment. You can still use your own GitHub App under Advanced."
        : "GitHub isn't configured on this deployment.";
    }
    if (sharedInstall) sharedInstall.disabled = !view.canManage;
    setHidden([retry], !(retryId && view.shared.configured && view.canManage));
    setHidden([readonly], view.canManage);
    if (readonly) readonly.textContent = "Only a workspace owner or admin can connect GitHub or change it.";

    // What is connected, by either app.
    setHidden([connected], !view.hasConnections);
    if (view.hasConnections) {
      paintInstallations(view);
      paintRepos(view);
      if (queueLine) {
        queueLine.textContent = `Queued: ${view.queued} · failed for good: ${view.dead}`;
      }
    }

    // Advanced: the workspace's own app.
    setHidden([advanced], !view.advanced);
    if (advanced && view.state === "connected") advanced.open = true;
    setHidden([setup], view.state !== "setup");
    setHidden([own], view.state !== "connected");
    if (view.state === "setup") {
      if (create) create.disabled = !view.canManage;
      for (const control of [owner, orgField]) if (control) control.disabled = !view.canManage;
      if (orgField) orgField.disabled = !view.canManage || !(owner && owner.checked);
    }
    if (view.state === "connected") {
      paintApp(view);
      if (add) add.disabled = !view.canManage;
      if (remove) remove.disabled = !view.canManage;
    }
  };

  const paintApp = (view) => {
    if (!appLine) return;
    appLine.replaceChildren();
    const text = doc.createElement("p");
    text.textContent = `The app is ${appSummaryLine(view)}.`;
    appLine.appendChild(text);
    if (view.app && view.app.htmlUrl) {
      const link = doc.createElement("a");
      link.setAttribute("href", view.app.htmlUrl);
      link.setAttribute("rel", "noopener noreferrer");
      link.textContent = "Open the app on GitHub";
      appLine.appendChild(link);
    }
  };

  const paintInstallations = (view) => {
    if (!installRows) return;
    installRows.replaceChildren();
    for (const install of view.installations) {
      const li = doc.createElement("li");
      li.textContent = install.line;
      if (install.manageUrl) {
        const gap = doc.createElement("span");
        gap.textContent = " · ";
        li.appendChild(gap);
        const link = doc.createElement("a");
        link.setAttribute("href", install.manageUrl);
        link.setAttribute("rel", "noopener noreferrer");
        link.textContent = "Manage on GitHub";
        li.appendChild(link);
      }
      installRows.appendChild(li);
    }
    if (!view.installations.length) {
      const li = doc.createElement("li");
      li.textContent = "No account has installed the app yet.";
      installRows.appendChild(li);
    }
  };

  const paintRepos = (view) => {
    if (!repoRows) return;
    repoRows.replaceChildren();
    if (!view.repos.length) {
      const tr = doc.createElement("tr");
      const td = doc.createElement("td");
      td.setAttribute("colspan", "7");
      td.textContent = "No repositories yet. Add them on GitHub's install page.";
      tr.appendChild(td);
      repoRows.appendChild(tr);
      return;
    }
    for (const repo of view.repos) {
      const tr = doc.createElement("tr");
      tr.appendChild(cell(doc, repo.name, "Repository"));
      tr.appendChild(cell(doc, repo.project, "Project"));
      tr.appendChild(cell(doc, repo.scope, "Scope"));
      const toggleCell = cell(doc, "", "Included");
      const box = doc.createElement("input");
      box.setAttribute("type", "checkbox");
      box.setAttribute("aria-label", `Include ${repo.name}`);
      box.checked = repo.included;
      box.disabled = !view.canManage;
      box.addEventListener("change", async () => {
        const want = box.checked;
        box.disabled = true;
        try {
          const request = repoRequest(repo.repoId, { included: want });
          await api(request.path, { method: request.method, body: request.body });
          await refresh();
        } catch (error) {
          box.checked = !want;
          box.disabled = !view.canManage;
          notify(status, githubErrorMessage(error, "change that repository"), "bad");
        }
      });
      toggleCell.appendChild(box);
      tr.appendChild(toggleCell);
      tr.appendChild(cell(doc, repo.backfill, "Backfill"));
      tr.appendChild(cell(doc, repo.lastWebhook || "none yet", "Last webhook"));
      tr.appendChild(cell(doc, repo.error ? `${repo.error}${repo.errorAt ? ` (${repo.errorAt})` : ""}` : "", "Last error"));
      repoRows.appendChild(tr);
    }
  };

  let current = null;
  const refresh = async () => {
    const data = await api(GITHUB_PATH);
    current = githubView(data);
    paint(current);
    return current;
  };

  // Writes: each one re-reads the page after it lands, so the page shows
  // what the server now holds and not what the button last said.

  // The shared app: a link the browser follows, only to github.com.
  const follow = async (button, request, what) => {
    button.disabled = true;
    try {
      const answer = await api(request.path, { method: request.method, body: request.body });
      if (!isGithubLink(answer && answer.url)) throw new Error("GitHub did not return a link Living Brain can follow.");
      deps.location.assign(answer.url);
    } catch (error) {
      button.disabled = false;
      notify(status, githubErrorMessage(error, what), "bad");
    }
  };
  if (sharedInstall) {
    sharedInstall.addEventListener("click", () => follow(sharedInstall, sharedInstallRequest(), "start the install"));
  }
  if (retry) {
    retry.addEventListener("click", () => {
      if (retryId) return follow(retry, sharedVerifyRequest(retryId), "finish connecting");
      return undefined;
    });
  }
  if (create) {
    create.addEventListener("click", async () => {
      const choice = ownerChoice(owner && owner.checked ? "org" : "user", orgField ? orgField.value : "");
      if (!choice.ok) {
        notify(status, choice.error, "bad");
        if (orgField) orgField.focus();
        return;
      }
      create.disabled = true;
      try {
        const request = manifestRequest(choice.body);
        const answer = await api(request.path, { method: request.method, body: request.body });
        const form = buildManifestForm(doc, { action: answer.action, manifestJson: answer.manifest_json });
        doc.body.appendChild(form);
        form.submit();
      } catch (error) {
        create.disabled = false;
        notify(status, githubErrorMessage(error, "start the GitHub app"), "bad");
      }
    });
  }
  if (owner && orgField) {
    const sync = () => {
      orgField.disabled = !owner.checked;
    };
    for (const radio of doc.querySelectorAll('input[name="github-owner"]')) {
      radio.addEventListener("change", sync);
    }
    sync();
  }
  if (add) {
    add.addEventListener("click", async () => {
      add.disabled = true;
      try {
        const request = installRequest();
        const answer = await api(request.path, { method: request.method });
        if (!isGithubLink(answer && answer.url)) throw new Error("GitHub did not return an install link.");
        deps.location.assign(answer.url);
      } catch (error) {
        add.disabled = false;
        notify(status, githubErrorMessage(error, "start the install"), "bad");
      }
    });
  }
  if (remove) {
    remove.addEventListener("click", async () => {
      const ok = await confirm({
        title: "Remove the GitHub app?",
        body: "Living Brain forgets the app's credentials and stops every repository it reaches. The app stays on GitHub until you delete it there.",
        confirm: "Remove",
      });
      if (!ok) return;
      remove.disabled = true;
      try {
        const request = removeAppRequest();
        const answer = await api(request.path, { method: request.method });
        const next = (answer && answer.next_step) || {};
        if (removed) {
          removed.replaceChildren();
          const text = doc.createElement("p");
          text.textContent = String(next.message || "The app is removed here. Delete it on GitHub too.");
          removed.appendChild(text);
          if (isGithubLink(next.url)) {
            const link = doc.createElement("a");
            link.setAttribute("href", next.url);
            link.setAttribute("rel", "noopener noreferrer");
            link.textContent = "Open the app on GitHub";
            removed.appendChild(link);
          }
          removed.hidden = false;
        }
        await refresh();
      } catch (error) {
        remove.disabled = false;
        notify(status, githubErrorMessage(error, "remove the app"), "bad");
      }
    });
  }

  await refresh();
  return current;
}

/** Shows or hides several elements that may be missing from the page. */
function setHidden(elements, hidden) {
  for (const element of elements) if (element) element.hidden = hidden;
}

/** A table cell with its column label (for narrow screens) and its text. */
function cell(doc, text, label) {
  const td = doc.createElement("td");
  td.setAttribute("data-label", label);
  td.textContent = String(text == null ? "" : text);
  return td;
}
