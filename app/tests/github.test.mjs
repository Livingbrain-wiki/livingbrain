// The GitHub tab in Settings → Integrations (`assets/github.js`), mounted on a
// recording DOM. The tests pin the request each button sends, which block is
// up for each state, the manifest form's shape, and the outcome banner. They
// assert how content reaches the DOM (`createElement`, `setAttribute`,
// `textContent`), never through markup strings.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  ownerChoice,
  manifestRequest,
  repoRequest,
  removeAppRequest,
  isGithubLink,
  buildManifestForm,
  outcomeBanner,
  scopeLabel,
  backfillText,
  githubView,
  mountGithub,
  sharedInstallRequest,
  sharedVerifyRequest,
  retryInstallation,
} from "../assets/github.js";

/* ------------------------------------------------------------ a recording DOM */

function element(tag, doc) {
  const node = {
    tag,
    doc,
    attrs: {},
    children: [],
    listeners: {},
    hidden: false,
    disabled: false,
    checked: false,
    value: "",
    name: "",
    _text: "",
    submitted: 0,
    setAttribute(name, value) {
      this.attrs[name] = String(value);
      if (name === "name") this.name = String(value);
    },
    getAttribute(name) {
      return name in this.attrs ? this.attrs[name] : null;
    },
    appendChild(child) {
      this.children.push(child);
      return child;
    },
    replaceChildren() {
      this.children = [];
      this._text = "";
    },
    addEventListener(type, handler) {
      (this.listeners[type] = this.listeners[type] || []).push(handler);
    },
    async dispatch(type) {
      for (const handler of this.listeners[type] || []) await handler({ target: this });
    },
    submit() {
      this.submitted += 1;
    },
    focus() {},
    get textContent() {
      return this._text + this.children.map((child) => child.textContent).join("");
    },
    set textContent(value) {
      this._text = String(value == null ? "" : value);
      this.children = [];
    },
  };
  return node;
}

/** A document with the settings page's GitHub ids, plus a body to append to. */
function githubDocument() {
  const registry = new Map();
  const add = (id, tag = "div", extra = {}) => {
    const node = element(tag, doc);
    node.attrs.id = id;
    Object.assign(node, extra);
    registry.set(id, node);
    return node;
  };
  const doc = {
    registry,
    body: element("body", null),
    createElement: (tag) => element(tag, doc),
    querySelector(selector) {
      if (selector.startsWith("#")) return registry.get(selector.slice(1)) || null;
      return null;
    },
    querySelectorAll(selector) {
      const match = /^input\[name="([^"]+)"\]$/.exec(selector);
      if (!match) return [];
      return [...registry.values()].filter((node) => node.tag === "input" && node.name === match[1]);
    },
  };
  for (const id of [
    "github-banner",
    "github-status",
    "github-readonly",
    "github-removed",
    "github-queue",
    "github-app",
    "github-installs",
    "github-repos",
  ]) {
    add(id);
  }
  add("github-setup");
  add("github-connected");
  add("github-shared");
  add("github-shared-off", "p");
  add("github-shared-install", "button");
  add("github-advanced", "details");
  add("github-own");
  add("github-retry", "button");
  add("github-org", "input");
  add("github-create", "button");
  add("github-add", "button");
  add("github-remove", "button");
  add("github-owner-user", "input", { name: "github-owner", value: "user", checked: true });
  add("github-owner-org", "input", { name: "github-owner", value: "org" });
  return doc;
}

/** The deps `mountGithub` takes, with a recorded API. */
function deps(doc, { search = "", answers = {}, confirm = async () => true } = {}) {
  const calls = [];
  const notes = [];
  const location = {
    search,
    pathname: "/settings.html",
    assigned: [],
    assign(url) {
      this.assigned.push(url);
    },
  };
  const history = { replaced: [], replaceState(_s, _t, url) { this.replaced.push(url); } };
  const api = async (path, options = {}) => {
    const method = options.method || "GET";
    calls.push({ method, path, body: options.body });
    const key = `${method} ${path}`;
    const answer = answers[key];
    if (answer instanceof Error) throw answer;
    return typeof answer === "function" ? answer(options) : answer;
  };
  const notify = (node, text, tone) => {
    notes.push({ node, text, tone });
    node.textContent = text;
  };
  return {
    deps: { api, confirm, notify, document: doc, location, history },
    calls,
    notes,
    location,
    history,
  };
}

const NOW = Date.UTC(2026, 9, 15) / 1000;

const OWNER_EMPTY = {
  configured: true,
  can_manage: true,
  app: null,
  installations: [],
  repos: [],
  queue: { queued: 0, dead: 0 },
};

const CONNECTED = {
  configured: true,
  can_manage: true,
  app: {
    app_id: 4242,
    slug: "living-brain-world-360-ab12",
    client_id: "Iv1.x",
    html_url: "https://github.com/apps/living-brain-world-360-ab12",
    install_url: "https://github.com/apps/living-brain-world-360-ab12/installations/new",
  },
  installations: [
    { installation_id: 99, account: { login: "World-360", type: "Organization" }, status: "active", bound_by: "m1" },
  ],
  repos: [
    {
      repo_id: 5,
      installation_id: 99,
      full_name: "World-360/EarthOS.world",
      project: "EarthOS.world",
      scope: "shared",
      included: true,
      active: true,
      backfill: { state: "running", items: 120, batches: 4, waiting_until: null },
      last_webhook_at: null,
      last_error: null,
    },
    {
      repo_id: 6,
      installation_id: 99,
      full_name: "World-360/website",
      project: "website",
      scope: "user:m1",
      included: false,
      active: false,
      backfill: { state: "pending", items: 0, batches: 0, waiting_until: null },
      last_webhook_at: 1760000000,
      last_error: "GitHub answered 502",
      last_error_at: 1760000100,
    },
  ],
  queue: { queued: 2, dead: 1 },
};

/* ------------------------------------------------------------ pure helpers */

test("the owner choice builds the manifest body, and refuses a bad organization login", () => {
  assert.deepEqual(ownerChoice("user"), { ok: true, body: { owner: "user" } });
  assert.deepEqual(ownerChoice("org", " World-360 "), { ok: true, body: { owner: "org", org: "World-360" } });
  for (const bad of ["", "-world", "world-", "a b", "x".repeat(40), "a/../b"]) {
    const choice = ownerChoice("org", bad);
    assert.equal(choice.ok, false, bad);
    assert.match(choice.error, /login/);
  }
  assert.deepEqual(manifestRequest({ owner: "user" }), {
    method: "POST",
    path: "/v1/github/app/manifest",
    body: { owner: "user" },
  });
  assert.equal(repoRequest(5, { included: false }).path, "/v1/github/repos/5");
  assert.equal(repoRequest(5, { included: false }).method, "PATCH");
  assert.equal(removeAppRequest().method, "DELETE");
});

test("a link from the server is followed only when it is a github.com URL", () => {
  assert.equal(isGithubLink("https://github.com/settings/apps/new?state=abc"), true);
  assert.equal(isGithubLink("https://github.com.evil.example/x"), false);
  assert.equal(isGithubLink("http://github.com/x"), false);
  assert.equal(isGithubLink("https://github.com/x\"><script>"), false);
  assert.equal(isGithubLink(null), false);
});

test("the manifest form: POST to GitHub, one hidden field named manifest, built with elements", () => {
  const doc = githubDocument();
  const form = buildManifestForm(doc, {
    action: "https://github.com/organizations/World-360/settings/apps/new?state=abc",
    manifestJson: '{"name":"Living Brain (W) 1a2b","hook_attributes":{"active":true}}',
  });
  assert.equal(form.tag, "form");
  assert.equal(form.getAttribute("method"), "POST");
  assert.equal(form.getAttribute("action"), "https://github.com/organizations/World-360/settings/apps/new?state=abc");
  assert.equal(form.children.length, 1, "exactly one field");
  const [field] = form.children;
  assert.equal(field.tag, "input");
  assert.equal(field.getAttribute("type"), "hidden");
  assert.equal(field.getAttribute("name"), "manifest");
  assert.equal(field.value, '{"name":"Living Brain (W) 1a2b","hook_attributes":{"active":true}}');

  assert.throws(() => buildManifestForm(doc, { action: "https://evil.example/", manifestJson: "{}" }), /manifest form/);
  assert.throws(() => buildManifestForm(doc, { action: "https://github.com/x", manifestJson: "" }), /manifest form/);
});

test("the outcome banner: every outcome and reason in docs/github.md has a plain sentence", () => {
  assert.equal(outcomeBanner(""), null);
  assert.equal(outcomeBanner("?tab=x"), null);
  assert.equal(outcomeBanner("?github=unknown"), null, "an unknown outcome names nothing");
  assert.equal(outcomeBanner("?github=installed").tone, "good");
  assert.match(outcomeBanner("?github=installed").text, /GitHub is connected/);
  assert.equal(outcomeBanner("?github=installed&reason=repos").tone, "warn");
  assert.equal(outcomeBanner("?github=installed&reason=suspended").tone, "warn");
  assert.equal(outcomeBanner("?github=requested").tone, "warn");
  assert.equal(outcomeBanner("?github=created").tone, "warn");
  for (const reason of [
    "bad-request",
    "signed-out",
    "not-configured",
    "state",
    "conversion",
    "app-exists",
    "app-id-taken",
    "forbidden",
    "store",
    "no-app",
    "github",
    "not-confirmed",
    "bound-elsewhere",
    "refused",
    "unclaimed",
    "not-verified",
    "not-installer",
    "installer-unknown",
  ]) {
    const banner = outcomeBanner(`?github=error&reason=${reason}`);
    assert.equal(banner.tone, "bad", reason);
    assert.ok(banner.text.length > 10, reason);
  }
  // A reason nobody listed never reaches the page verbatim.
  const odd = outcomeBanner("?github=error&reason=%3Cimg%20src%3Dx%3E");
  assert.equal(odd.tone, "bad");
  assert.ok(!odd.text.includes("<img"));
});

test("scope and backfill read as sentences a person can act on", () => {
  assert.equal(scopeLabel("shared"), "Shared");
  assert.equal(scopeLabel("user:m1"), "Personal");
  assert.match(backfillText({ state: "done", items: 10, batches: 2 }, NOW), /done: 10 items in 2 batches/);
  assert.match(backfillText({ state: "running", items: 3, batches: 1 }, NOW), /Backfilling: 3 items/);
  assert.match(backfillText({ state: "pending" }, NOW), /starts shortly/);
  const waiting = backfillText({ state: "running", items: 3, batches: 1, waiting_until: NOW + 600 }, NOW);
  assert.match(waiting, /Waiting for GitHub/);
});

test("the view model: the state follows whether an app exists, and repos keep their order", () => {
  const empty = githubView(OWNER_EMPTY, NOW);
  assert.equal(empty.state, "setup");
  assert.equal(empty.canManage, true);
  const connected = githubView(CONNECTED, NOW);
  assert.equal(connected.state, "connected");
  assert.deepEqual(connected.repos.map((repo) => repo.name), ["World-360/EarthOS.world", "World-360/website"]);
  assert.equal(connected.repos[0].project, "EarthOS.world");
  assert.equal(connected.repos[1].scope, "Personal");
  assert.equal(connected.repos[1].error, "GitHub answered 502");
  assert.equal(connected.installations[0].line, "World-360 · Organization · active");
  assert.equal(connected.app.htmlUrl, "https://github.com/apps/living-brain-world-360-ab12");
  assert.equal(githubView({ ...CONNECTED, app: { ...CONNECTED.app, html_url: "https://evil.example" } }, NOW).app.htmlUrl, "");
  assert.equal(githubView({ configured: false }, NOW).configured, false);
});

test("nothing in the GitHub tab reaches for markup strings or storage", () => {
  const code = readFileSync(new URL("../assets/github.js", import.meta.url), "utf8")
    .split("\n")
    .filter((line) => !line.trim().startsWith("//"))
    .join("\n");
  for (const forbidden of ["innerHTML", "insertAdjacentHTML", "document.write", "localStorage", "sessionStorage"]) {
    assert.ok(!code.includes(forbidden), `github.js uses ${forbidden}`);
  }
});

test("every element the tab paints is in settings.html", () => {
  const html = readFileSync(new URL("../settings.html", import.meta.url), "utf8");
  const code = readFileSync(new URL("../assets/github.js", import.meta.url), "utf8");
  const ids = new Set([...code.matchAll(/q\("#([a-z-]+)"\)/g)].map((m) => m[1]));
  assert.ok(ids.size >= 15, "the tab reads its elements");
  for (const id of ids) assert.match(html, new RegExp(`id="${id}"`), `settings.html has #${id}`);
  assert.match(html, /id="github"[^>]*aria-labelledby="github-h"/);
});

/* ---------------------------------------------------------------- the tab */

test("owner, not connected: the create form is live and no request is sent until it is submitted", async () => {
  const doc = githubDocument();
  const { deps: d, calls } = deps(doc, { answers: { "GET /v1/github": OWNER_EMPTY } });
  await mountGithub(d);
  assert.equal(doc.registry.get("github-setup").hidden, false);
  assert.equal(doc.registry.get("github-connected").hidden, true);
  assert.equal(doc.registry.get("github-readonly").hidden, true);
  assert.equal(doc.registry.get("github-create").disabled, false);
  assert.equal(doc.registry.get("github-org").disabled, true, "the login waits for the org choice");
  assert.deepEqual(calls.map((c) => `${c.method} ${c.path}`), ["GET /v1/github"]);
});

test("creating the app posts the owner choice, then submits the manifest form to GitHub", async () => {
  const doc = githubDocument();
  const action = "https://github.com/organizations/World-360/settings/apps/new?state=s1";
  const { deps: d, calls } = deps(doc, {
    answers: {
      "GET /v1/github": OWNER_EMPTY,
      "POST /v1/github/app/manifest": {
        action,
        method: "POST",
        field: "manifest",
        manifest: { name: "Living Brain (World-360) 1a2b" },
        manifest_json: '{"name":"Living Brain (World-360) 1a2b"}',
        expires_in: 900,
      },
    },
  });
  await mountGithub(d);
  // Choose the organization and type its login.
  doc.registry.get("github-owner-org").checked = true;
  doc.registry.get("github-owner-org").dispatch("change");
  doc.registry.get("github-owner-user").checked = false;
  doc.registry.get("github-org").value = "World-360";
  assert.equal(doc.registry.get("github-org").disabled, false);
  await doc.registry.get("github-create").dispatch("click");

  const post = calls.find((c) => c.method === "POST");
  assert.equal(post.path, "/v1/github/app/manifest");
  assert.deepEqual(post.body, { owner: "org", org: "World-360" });

  const [form] = doc.body.children;
  assert.equal(form.getAttribute("method"), "POST");
  assert.equal(form.getAttribute("action"), action);
  assert.equal(form.children.length, 1);
  assert.equal(form.children[0].getAttribute("name"), "manifest");
  assert.equal(form.children[0].value, '{"name":"Living Brain (World-360) 1a2b"}');
  assert.equal(form.submitted, 1, "the form is submitted once");
});

test("a bad organization login is refused in the page and sends nothing", async () => {
  const doc = githubDocument();
  const { deps: d, calls, notes } = deps(doc, { answers: { "GET /v1/github": OWNER_EMPTY } });
  await mountGithub(d);
  doc.registry.get("github-owner-org").checked = true;
  doc.registry.get("github-org").value = "-nope";
  await doc.registry.get("github-create").dispatch("click");
  assert.equal(calls.filter((c) => c.method === "POST").length, 0);
  assert.equal(notes.at(-1).tone, "bad");
  assert.match(notes.at(-1).text, /login/);
});

test("a member who is not an owner or admin sees read-only text and no live controls", async () => {
  const doc = githubDocument();
  const { deps: d } = deps(doc, { answers: { "GET /v1/github": { ...OWNER_EMPTY, can_manage: false } } });
  await mountGithub(d);
  assert.equal(doc.registry.get("github-readonly").hidden, false);
  assert.match(doc.registry.get("github-readonly").textContent, /Only a workspace owner or admin/);
  assert.equal(doc.registry.get("github-create").disabled, true);
});

test("connected: the app, its accounts, the repositories with scope and backfill, and the queue", async () => {
  const doc = githubDocument();
  const { deps: d } = deps(doc, { answers: { "GET /v1/github": CONNECTED } });
  await mountGithub(d);
  assert.equal(doc.registry.get("github-setup").hidden, true);
  assert.equal(doc.registry.get("github-connected").hidden, false);
  assert.match(doc.registry.get("github-app").textContent, /living-brain-world-360-ab12/);
  assert.match(doc.registry.get("github-installs").textContent, /World-360 · Organization · active/);
  const rows = doc.registry.get("github-repos").children;
  assert.equal(rows.length, 2);
  assert.match(rows[0].textContent, /World-360\/EarthOS\.world/);
  assert.match(rows[0].textContent, /EarthOS\.world/);
  assert.match(rows[0].textContent, /Backfilling: 120 items in 4 batches/);
  assert.match(rows[1].textContent, /GitHub answered 502/);
  const boxes = rows.map((row) => row.children[3].children[0]);
  assert.equal(boxes[0].checked, true);
  assert.equal(boxes[1].checked, false);
  assert.match(doc.registry.get("github-queue").textContent, /Queued: 2 · failed for good: 1/);
  // There is no button that starts a backfill.
  for (const id of ["github-create", "github-add", "github-remove"]) {
    assert.doesNotMatch(String(doc.registry.get(id).textContent), /backfill/i);
  }
});

test("including or excluding a repository is one PATCH, then the page re-reads", async () => {
  const doc = githubDocument();
  const answers = { "GET /v1/github": CONNECTED, "PATCH /v1/github/repos/6": { repo_id: 6 } };
  const { deps: d, calls } = deps(doc, { answers });
  await mountGithub(d);
  const box = doc.registry.get("github-repos").children[1].children[3].children[0];
  box.checked = true;
  await box.dispatch("change");
  const patch = calls.find((c) => c.method === "PATCH");
  assert.equal(patch.path, "/v1/github/repos/6");
  assert.deepEqual(patch.body, { included: true });
  assert.equal(calls.filter((c) => c.method === "GET").length, 2, "the page re-reads after the write");
});

test("a failed repository write puts the toggle back and says why", async () => {
  const doc = githubDocument();
  const answers = {
    "GET /v1/github": CONNECTED,
    "PATCH /v1/github/repos/6": Object.assign(new Error("forbidden"), { status: 403 }),
  };
  const { deps: d, notes } = deps(doc, { answers });
  await mountGithub(d);
  const box = doc.registry.get("github-repos").children[1].children[3].children[0];
  box.checked = true;
  await box.dispatch("change");
  assert.equal(box.checked, false);
  assert.equal(notes.at(-1).tone, "bad");
  assert.match(notes.at(-1).text, /Only a workspace owner or admin can change that repository/);
});

test("Add another account opens the install link GitHub returns, and only a GitHub one", async () => {
  const doc = githubDocument();
  const answers = {
    "GET /v1/github": CONNECTED,
    "POST /v1/github/app/install": { url: "https://github.com/apps/living-brain-world-360-ab12/installations/new?state=s2" },
  };
  const { deps: d, location } = deps(doc, { answers });
  await mountGithub(d);
  await doc.registry.get("github-add").dispatch("click");
  assert.deepEqual(location.assigned, [
    "https://github.com/apps/living-brain-world-360-ab12/installations/new?state=s2",
  ]);

  const other = githubDocument();
  const bad = deps(other, {
    answers: { "GET /v1/github": CONNECTED, "POST /v1/github/app/install": { url: "https://evil.example/x" } },
  });
  await mountGithub(bad.deps);
  await other.registry.get("github-add").dispatch("click");
  assert.deepEqual(bad.location.assigned, [], "a link off GitHub is not followed");
});

test("Remove asks first, forgets the app, and shows the next step on GitHub", async () => {
  const doc = githubDocument();
  let asked = 0;
  const answers = {
    "GET /v1/github": CONNECTED,
    "DELETE /v1/github/app": {
      removed: true,
      next_step: {
        message: "Living Brain has forgotten the app. Delete it on GitHub too: Advanced, then Delete GitHub App.",
        url: "https://github.com/apps/living-brain-world-360-ab12/advanced",
      },
    },
  };
  const { deps: d, calls } = deps(doc, {
    answers,
    confirm: async (options) => {
      asked += 1;
      assert.match(options.body, /stays on GitHub/);
      return true;
    },
  });
  await mountGithub(d);
  await doc.registry.get("github-remove").dispatch("click");
  assert.equal(asked, 1);
  assert.ok(calls.some((c) => c.method === "DELETE" && c.path === "/v1/github/app"));
  const removed = doc.registry.get("github-removed");
  assert.equal(removed.hidden, false);
  assert.match(removed.textContent, /Delete it on GitHub too/);
  const link = removed.children.find((child) => child.tag === "a");
  assert.equal(link.getAttribute("href"), "https://github.com/apps/living-brain-world-360-ab12/advanced");
});

test("Remove that the person cancels sends nothing", async () => {
  const doc = githubDocument();
  const { deps: d, calls } = deps(doc, {
    answers: { "GET /v1/github": CONNECTED },
    confirm: async () => false,
  });
  await mountGithub(d);
  await doc.registry.get("github-remove").dispatch("click");
  assert.equal(calls.filter((c) => c.method === "DELETE").length, 0);
});

test("the settings address with an outcome shows its banner once, and the address is tidied", async () => {
  const doc = githubDocument();
  const { deps: d, notes, history } = deps(doc, {
    search: "?github=error&reason=app-exists",
    answers: { "GET /v1/github": CONNECTED },
  });
  await mountGithub(d);
  const banner = doc.registry.get("github-banner");
  assert.equal(banner.textContent, "This workspace already has a GitHub app.");
  assert.equal(notes.find((n) => n.node === banner).tone, "bad");
  assert.deepEqual(history.replaced, ["/settings.html#github"]);
});

test("a configured-off server says so and paints no GitHub block", async () => {
  const doc = githubDocument();
  const { deps: d } = deps(doc, { answers: { "GET /v1/github": { configured: false, can_manage: true, queue: {} } } });
  await mountGithub(d);
  assert.match(doc.registry.get("github-status").textContent, /isn't available here yet/);
  assert.equal(doc.registry.get("github-setup").hidden, true);
  assert.equal(doc.registry.get("github-connected").hidden, true);
});

/* ------------------------------------------------------- the shared app */

const SHARED_EMPTY = {
  configured: true,
  can_manage: true,
  shared: { configured: true, slug: "living-brain", html_url: "https://github.com/apps/living-brain" },
  advanced: { configured: true },
  app: null,
  installations: [],
  repos: [],
  queue: { queued: 0, dead: 0 },
};

const SHARED_CONNECTED = {
  ...SHARED_EMPTY,
  installations: [
    {
      installation_id: 7001,
      app_kind: "shared",
      manage_url: "https://github.com/organizations/World-360/settings/installations/7001",
      account: { login: "World-360", type: "Organization" },
      status: "active",
      bound_by: "m1",
    },
  ],
  repos: [CONNECTED.repos[0]],
};

test("the shared requests and the retry address read only what they should", () => {
  assert.deepEqual(sharedInstallRequest(), { method: "POST", path: "/v1/github/shared/install" });
  assert.deepEqual(sharedVerifyRequest(7001), {
    method: "POST",
    path: "/v1/github/shared/verify",
    body: { installation_id: 7001 },
  });
  assert.equal(retryInstallation("?github=error&reason=installer-unknown&installation=7001"), 7001);
  assert.equal(retryInstallation("?github=error&reason=installer-unknown&installation=7001x"), null);
  assert.equal(retryInstallation("?github=error&reason=installer-unknown"), null);
  assert.equal(retryInstallation("?github=error&reason=state&installation=7001"), null);
  assert.equal(retryInstallation(""), null);
});

test("default view: the shared install button is up, and the own-app flow waits under Advanced", async () => {
  const doc = githubDocument();
  const { deps: d, calls } = deps(doc, { answers: { "GET /v1/github": SHARED_EMPTY } });
  const view = await mountGithub(d);
  assert.equal(view.shared.configured, true);
  assert.equal(doc.registry.get("github-shared").hidden, false);
  assert.equal(doc.registry.get("github-shared-install").disabled, false);
  assert.equal(doc.registry.get("github-shared-off").hidden, true);
  assert.equal(doc.registry.get("github-connected").hidden, true);
  // Advanced is there, closed, with the manifest form inside it.
  assert.equal(doc.registry.get("github-advanced").hidden, false);
  assert.notEqual(doc.registry.get("github-advanced").open, true);
  assert.equal(doc.registry.get("github-setup").hidden, false);
  assert.equal(doc.registry.get("github-own").hidden, true);
  assert.equal(doc.registry.get("github-retry").hidden, true);
  assert.deepEqual(calls.map((c) => `${c.method} ${c.path}`), ["GET /v1/github"]);
});

test("Install Living Brain on GitHub follows the server's link to github.com, never a form", async () => {
  const doc = githubDocument();
  const url = "https://github.com/apps/living-brain/installations/new?state=s1";
  const { deps: d, calls, location } = deps(doc, {
    answers: { "GET /v1/github": SHARED_EMPTY, "POST /v1/github/shared/install": { url } },
  });
  await mountGithub(d);
  await doc.registry.get("github-shared-install").dispatch("click");
  assert.ok(calls.some((c) => c.method === "POST" && c.path === "/v1/github/shared/install"));
  assert.deepEqual(location.assigned, [url]);
  assert.equal(doc.body.children.length, 0, "no form is built for the shared app");

  const other = githubDocument();
  const bad = deps(other, {
    answers: { "GET /v1/github": SHARED_EMPTY, "POST /v1/github/shared/install": { url: "https://evil.example/x" } },
  });
  await mountGithub(bad.deps);
  await other.registry.get("github-shared-install").dispatch("click");
  assert.deepEqual(bad.location.assigned, [], "a link off GitHub is not followed");
  assert.equal(bad.notes.at(-1).tone, "bad");
});

test("without the shared secrets: the page says GitHub isn't configured, and Advanced remains", async () => {
  const doc = githubDocument();
  const payload = { ...SHARED_EMPTY, shared: { configured: false, slug: null } };
  const { deps: d } = deps(doc, { answers: { "GET /v1/github": payload } });
  await mountGithub(d);
  assert.equal(doc.registry.get("github-shared").hidden, true);
  assert.equal(doc.registry.get("github-shared-off").hidden, false);
  assert.match(doc.registry.get("github-shared-off").textContent, /isn't available here yet/);
  assert.equal(doc.registry.get("github-advanced").hidden, false);
  assert.equal(doc.registry.get("github-setup").hidden, false);
});

test("connected through the shared app: accounts with a Manage on GitHub link, repositories, no own-app controls", async () => {
  const doc = githubDocument();
  const { deps: d } = deps(doc, { answers: { "GET /v1/github": SHARED_CONNECTED } });
  await mountGithub(d);
  assert.equal(doc.registry.get("github-connected").hidden, false);
  assert.equal(doc.registry.get("github-own").hidden, true);
  const [li] = doc.registry.get("github-installs").children;
  assert.match(li.textContent, /World-360 · Organization · active/);
  assert.doesNotMatch(li.textContent, /your own app/);
  const link = li.children.find((child) => child.tag === "a");
  assert.equal(link.getAttribute("href"), "https://github.com/organizations/World-360/settings/installations/7001");
  assert.equal(doc.registry.get("github-repos").children.length, 1);

  // A manage link off GitHub is dropped.
  const odd = githubView({
    ...SHARED_CONNECTED,
    installations: [{ ...SHARED_CONNECTED.installations[0], manage_url: "javascript:alert(1)" }],
  });
  assert.equal(odd.installations[0].manageUrl, "");
});

test("a member who is not an owner or admin cannot press Install", async () => {
  const doc = githubDocument();
  const { deps: d } = deps(doc, { answers: { "GET /v1/github": { ...SHARED_EMPTY, can_manage: false } } });
  await mountGithub(d);
  assert.equal(doc.registry.get("github-shared-install").disabled, true);
  assert.equal(doc.registry.get("github-readonly").hidden, false);
});

test("installer-unknown: the banner explains, and Finish connecting starts another OAuth hop", async () => {
  const doc = githubDocument();
  const url = "https://github.com/login/oauth/authorize?client_id=Iv23li&state=v2";
  const { deps: d, calls, location } = deps(doc, {
    search: "?github=error&reason=installer-unknown&installation=7001",
    answers: { "GET /v1/github": SHARED_EMPTY, "POST /v1/github/shared/verify": { url } },
  });
  await mountGithub(d);
  assert.match(doc.registry.get("github-banner").textContent, /who installed the app/);
  const retry = doc.registry.get("github-retry");
  assert.equal(retry.hidden, false);
  await retry.dispatch("click");
  const post = calls.find((c) => c.method === "POST");
  assert.equal(post.path, "/v1/github/shared/verify");
  assert.deepEqual(post.body, { installation_id: 7001 });
  assert.deepEqual(location.assigned, [url]);

  // Any other address: no retry button.
  const plain = githubDocument();
  const other = deps(plain, { search: "?github=installed", answers: { "GET /v1/github": SHARED_EMPTY } });
  await mountGithub(other.deps);
  assert.equal(plain.registry.get("github-retry").hidden, true);
});
