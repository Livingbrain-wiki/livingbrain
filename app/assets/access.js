// Agent accounts, access grants and the audit log (ADR 0004) — request
// builders, view models and painters. Elements and text nodes only, as
// everywhere in `app/`. An agent key reaches the page through exactly one
// function, `revealAgentKey`, from exactly one response (a mint or a rotate),
// and leaves it again with `clearAgentKey`; no list view model can carry one.

export const AGENTS_PATH = "/v1/agents";
export const GRANTS_PATH = "/v1/access/grants";
export const AUDIT_PATH = "/v1/audit";

function enc(value, what) {
  const text = String(value == null ? "" : value).trim();
  if (!text) throw new Error(`${what} is required`);
  return encodeURIComponent(text);
}

/* --------------------------------------------------------- requests */

export function agentListRequest() {
  return { method: "GET", path: AGENTS_PATH };
}

export function agentCreateRequest(name, description) {
  const label = String(name == null ? "" : name).trim();
  if (!label) throw new Error("an agent account needs a name");
  return {
    method: "POST",
    path: AGENTS_PATH,
    body: { name: label, description: String(description || "").trim() },
  };
}

export function agentUpdateRequest(id, patch) {
  const body = {};
  for (const key of ["name", "description", "disabled", "owner_id"]) {
    if (patch && patch[key] !== undefined) body[key] = patch[key];
  }
  return { method: "PATCH", path: `${AGENTS_PATH}/${enc(id, "an agent id")}`, body };
}

export function agentDeleteRequest(id) {
  return { method: "DELETE", path: `${AGENTS_PATH}/${enc(id, "an agent id")}` };
}

export function agentKeysRequest(id) {
  return { method: "GET", path: `${AGENTS_PATH}/${enc(id, "an agent id")}/keys` };
}

/** Mint a key. `days` empty means no expiry; otherwise 1–366. */
export function agentKeyMintRequest(id, name, days) {
  const body = { name: String(name || "").trim() };
  const n = Number(days);
  if (days !== "" && days != null) {
    if (!Number.isInteger(n) || n < 1 || n > 366) throw new Error("expiry is 1 to 366 days");
    body.expires_in_days = n;
  }
  return { method: "POST", path: `${AGENTS_PATH}/${enc(id, "an agent id")}/keys`, body };
}

export function agentKeyRotateRequest(id, prefix) {
  return {
    method: "POST",
    path: `${AGENTS_PATH}/${enc(id, "an agent id")}/keys/${enc(prefix, "a key prefix")}/rotate`,
  };
}

export function agentKeyRevokeRequest(id, prefix) {
  return {
    method: "DELETE",
    path: `${AGENTS_PATH}/${enc(id, "an agent id")}/keys/${enc(prefix, "a key prefix")}`,
  };
}

export function grantListRequest() {
  return { method: "GET", path: GRANTS_PATH };
}

const SEGMENT = /^[A-Za-z0-9._-]+$/;

/** A folder as a person types it: `coding-agents/claude/`. Throws on a
 * segment outside the alphabet, `.`/`..`, or a `--` (the slug separator). */
export function parseFolder(text) {
  const segments = String(text || "")
    .split("/")
    .map((s) => s.trim())
    .filter(Boolean);
  for (const segment of segments) {
    if (!SEGMENT.test(segment) || segment === "." || segment === ".." || segment.includes("--")) {
      throw new Error(`"${segment}" is not a folder name`);
    }
  }
  return segments.join("/");
}

export function grantCreateRequest({ principal, space, folder, kind, permission, effect }) {
  const who = String(principal || "").trim();
  if (!/^(member|agent|role):.+$/.test(who)) throw new Error("choose who gets access");
  const where = String(space || "").trim();
  if (where !== "shared" && !where.startsWith("personal:")) throw new Error("choose a space");
  const perm = ["read", "write", "admin"].includes(permission) ? permission : "read";
  const body = { principal: who, space: where, path: parseFolder(folder), permission: perm };
  if (kind && kind !== "*") body.kind = kind;
  if (effect === "deny") body.effect = "deny";
  return { method: "POST", path: GRANTS_PATH, body };
}

/** The most grants one batch creates or revokes (the server's cap too). */
export const MAX_BATCH = 100;

/** Above this many, the form asks before it creates them. */
export const CONFIRM_OVER = 10;

const KINDS = ["pages", "sources", "facts"];

function distinct(values) {
  return [...new Set((Array.isArray(values) ? values : []).map((v) => String(v).trim()).filter(Boolean))];
}

/** An expiry as the date input gives it (`2026-12-31`) to the end of that
 * day, UTC; empty is none. */
export function expiryOf(date) {
  const text = String(date || "").trim();
  if (!text) return null;
  if (!/^\d{4}-\d{2}-\d{2}$/.test(text) || Number.isNaN(Date.parse(`${text}T23:59:59Z`))) {
    throw new Error("the expiry is a date");
  }
  return `${text}T23:59:59Z`;
}

/**
 * The grant form's answer as grants: every chosen *who* × *space* ×
 * *folder* (none = the whole space) × *kind* (`*` = everything), with one
 * permission and an optional expiry. `permission: "deny"` is a deny.
 * Answers `{ grants, count, who, spaces, folders, kinds }`; throws with what
 * is missing.
 */
export function grantPlan({ principals, spaces, folders, kinds, permission, expires } = {}) {
  const who = distinct(principals);
  if (!who.length || who.some((p) => !/^(member|agent|role):.+$/.test(p))) {
    throw new Error("choose who gets access");
  }
  const where = distinct(spaces);
  if (!where.length || where.some((s) => s !== "shared" && !/^personal:.+$/.test(s))) {
    throw new Error("choose a space");
  }
  const paths = distinct((Array.isArray(folders) ? folders : []).map(parseFolder));
  const under = paths.length ? paths : [""];
  const chosen = distinct(kinds);
  if (!chosen.length) throw new Error("choose what it covers");
  const what = chosen.includes("*") ? ["*"] : chosen.filter((k) => KINDS.includes(k));
  if (!what.length) throw new Error("choose what it covers");
  const deny = permission === "deny";
  const perm = deny ? "read" : ["read", "write", "admin"].includes(permission) ? permission : "read";
  const expiresAt = expiryOf(expires);
  const grants = [];
  for (const principal of who) {
    for (const space of where) {
      for (const path of under) {
        for (const kind of what) {
          const body = { principal, space, path, kind, permission: perm };
          if (deny) body.effect = "deny";
          if (expiresAt) body.expires_at = expiresAt;
          grants.push(body);
        }
      }
    }
  }
  return {
    grants,
    count: grants.length,
    who: who.length,
    spaces: where.length,
    folders: under.length,
    kinds: what.length,
  };
}

function plural(n, one, many) {
  return `${n} ${n === 1 ? one : many}`;
}

/** "This creates 6 grants: 2 people × 1 space × 3 folders × 1 kind." */
export function grantPreview(plan) {
  if (!plan || !plan.count) return "";
  const head = plan.count === 1 ? "This creates 1 grant" : `This creates ${plan.count} grants`;
  const parts = [
    plural(plan.who, "grantee", "grantees"),
    plural(plan.spaces, "space", "spaces"),
    plan.folders === 1 ? "1 folder" : `${plan.folders} folders`,
    plural(plan.kinds, "kind", "kinds"),
  ];
  const over = plan.count > MAX_BATCH ? ` — more than the ${MAX_BATCH} one request makes; narrow it down` : "";
  return `${head}: ${parts.join(" × ")}${over}.`;
}

/** Whether the form should ask before creating `count` grants. */
export function needsConfirm(count) {
  return Number(count) > CONFIRM_OVER;
}

export function grantBatchRequest(grants) {
  const list = Array.isArray(grants) ? grants : [];
  if (!list.length) throw new Error("nothing to grant");
  if (list.length > MAX_BATCH) {
    throw new Error(`that is ${list.length} grants; one request makes at most ${MAX_BATCH}`);
  }
  return { method: "POST", path: `${GRANTS_PATH}/batch`, body: { grants: list } };
}

export function grantRevokeBatchRequest(ids) {
  const list = distinct(ids);
  if (!list.length) throw new Error("choose the grants to revoke");
  if (list.length > MAX_BATCH) throw new Error(`revoke at most ${MAX_BATCH} at once`);
  return { method: "POST", path: `${GRANTS_PATH}/batch/revoke`, body: { ids: list } };
}

export function agentBatchRequest(ids, disabled) {
  const list = distinct(ids);
  if (!list.length) throw new Error("choose the agent accounts");
  if (list.length > MAX_BATCH) throw new Error(`change at most ${MAX_BATCH} at once`);
  return { method: "POST", path: `${AGENTS_PATH}/batch`, body: { ids: list, disabled: Boolean(disabled) } };
}

/** The *who* choices: agents, other members, role groups. */
export function principalOptions(members, agents, selfId) {
  const options = [];
  for (const member of Array.isArray(members) ? members : []) {
    if (!member || !member.user_id || member.user_id === selfId) continue;
    options.push({
      value: `member:${member.user_id}`,
      label: String(member.name || member.user_id),
      group: "People",
      hint: member.role ? String(member.role) : "",
    });
  }
  for (const agent of Array.isArray(agents) ? agents : []) {
    if (!agent || !agent.id) continue;
    options.push({
      value: `agent:${agent.id}`,
      label: String(agent.name || agent.id),
      group: "Agent accounts",
      hint: agent.disabled ? "disabled" : "agent",
    });
  }
  for (const role of ["member", "admin", "viewer"]) {
    options.push({ value: `role:${role}`, label: `Everyone with the ${role} role`, group: "Role groups", hint: "" });
  }
  return options;
}

const MAX_FOLDERS = 300;

/**
 * The folder choices, from answers the app already reads (each filtered to
 * what the asker may see): `GET /v1/coding-agents/tree` (agent and project
 * folders), `GET /v1/pages` (the folders page slugs spell with `--`) and
 * `GET /v1/sources/projects` (`project/<name>` for sources). `segment` maps a
 * project name to its folder segment, as the compiler does (`projectSegment`).
 */
export function folderOptions({ tree, pages, projects } = {}, segment = (name) => name) {
  const seen = new Map();
  const offer = (path, group, hint) => {
    let value;
    try {
      value = parseFolder(path);
    } catch {
      return;
    }
    if (!value || seen.has(value) || seen.size >= MAX_FOLDERS) return;
    seen.set(value, { value, label: value, group, hint });
  };
  const agents = tree && Array.isArray(tree.agents) ? tree.agents : [];
  if (agents.length) offer("coding-agents", "Coding agents", "every agent");
  for (const agent of agents) {
    const folder = String((agent && (agent.folder || agent.agent)) || "");
    if (!folder) continue;
    offer(`coding-agents/${folder}`, "Coding agents", String(agent.agent || folder));
    for (const project of Array.isArray(agent.projects) ? agent.projects : []) {
      const name = project && project.project ? String(project.project) : "";
      offer(`coding-agents/${folder}/${segment(name)}`, "Coding agents", name || "no project");
    }
  }
  const rows = pages && Array.isArray(pages.pages) ? pages.pages : [];
  const counts = new Map();
  for (const row of rows) {
    const parts = String((row && row.slug) || "").split("--");
    for (let n = 1; n < parts.length; n += 1) {
      const path = parts.slice(0, n).join("/");
      counts.set(path, (counts.get(path) || 0) + 1);
    }
  }
  for (const [path, n] of [...counts.entries()].sort((a, b) => a[0].localeCompare(b[0]))) {
    offer(path, "Page folders", plural(n, "page", "pages"));
  }
  const list = projects && Array.isArray(projects.projects) ? projects.projects : [];
  for (const entry of list) {
    const name = entry && entry.project ? String(entry.project) : "";
    if (name) offer(`project/${name}`, "Source projects", plural(Number(entry.sources) || 0, "source", "sources"));
  }
  return [...seen.values()];
}

/** The grants list's filter choices, from the grants themselves. */
export function grantFilterOptions(views, members, agents) {
  const list = Array.isArray(views) ? views : [];
  const who = distinct(list.map((v) => v.principal)).map((value) => ({
    value,
    label: principalLabel(value, members, agents),
  }));
  const where = [];
  for (const space of distinct(list.map((v) => v.space))) {
    where.push({ value: `space:${space}`, label: space === "shared" ? "Shared space" : space });
  }
  for (const path of distinct(list.map((v) => v.path))) {
    where.push({ value: `folder:${path}`, label: `${path}/` });
  }
  const what = distinct(list.map((v) => v.kind)).map((value) => ({
    value,
    label: value === "*" ? "everything" : value,
  }));
  return { who, where, what };
}

/** The grants a filter keeps. Empty fields keep everything. */
export function filterGrants(views, filter = {}) {
  const list = Array.isArray(views) ? views : [];
  return list.filter((view) => {
    if (filter.who && view.principal !== filter.who) return false;
    if (filter.what && view.kind !== filter.what) return false;
    if (filter.where) {
      if (filter.where.startsWith("space:") && view.space !== filter.where.slice(6)) return false;
      if (filter.where.startsWith("folder:")) {
        const folder = filter.where.slice(7);
        if (view.path !== folder && !view.path.startsWith(`${folder}/`)) return false;
      }
    }
    return true;
  });
}

/** Which of `ids` are still in `views`: a selection kept across a reload or
 * a filter change only holds rows that are there. */
export function keepSelected(selected, views) {
  const present = new Set((Array.isArray(views) ? views : []).map((v) => v.id));
  return new Set([...(selected || [])].filter((id) => present.has(id)));
}

/** "2 selected" / "Revoke 2 grants". */
export function selectionText(n, noun) {
  return n ? `${plural(n, noun, `${noun}s`)} selected` : "None selected";
}

export function grantDeleteRequest(id) {
  return { method: "DELETE", path: `${GRANTS_PATH}/${enc(id, "a grant id")}` };
}

export function pageAccessRequest(slug) {
  return { method: "GET", path: `/v1/access/pages/${enc(slug, "a page")}` };
}

export function auditRequest(before) {
  const cursor = String(before || "").trim();
  return { method: "GET", path: cursor ? `${AUDIT_PATH}?before=${encodeURIComponent(cursor)}` : AUDIT_PATH };
}

/* ------------------------------------------------------- view models */

/** One agent, named fields only. */
export function agentView(entry) {
  return {
    id: String((entry && entry.id) || ""),
    name: String((entry && entry.name) || ""),
    description: String((entry && entry.description) || ""),
    ownerId: String((entry && entry.owner_id) || ""),
    disabled: Boolean(entry && entry.disabled),
  };
}

export function agentViews(response) {
  const list = Array.isArray(response && response.agents) ? response.agents : [];
  return list.map(agentView);
}

/** One key, as a list shows it: never the key. */
export function keyView(entry) {
  const day = (v) => (v ? String(v).slice(0, 10) : "");
  return {
    prefix: String((entry && entry.prefix) || ""),
    name: String((entry && entry.name) || ""),
    created: day(entry && entry.created_at),
    expires: day(entry && entry.expires_at),
    lastUsed: day(entry && entry.last_used_at),
    revoked: Boolean(entry && entry.revoked_at),
  };
}

export function keyViews(response) {
  const list = Array.isArray(response && response.keys) ? response.keys : [];
  return list.map(keyView);
}

/** The one place a key value is read: a mint or rotate response. */
export function mintedKey(response) {
  return { ...keyView(response), key: String((response && response.key) || "") };
}

export function grantView(entry) {
  return {
    id: String((entry && entry.id) || ""),
    principal: String((entry && entry.principal) || ""),
    space: String((entry && entry.space) || ""),
    path: String((entry && entry.path) || ""),
    folder: String((entry && entry.path) || "") || "(everything)",
    kind: String((entry && entry.kind) || "*"),
    permission: String((entry && entry.permission) || ""),
    effect: String((entry && entry.effect) || "allow"),
    expires: entry && entry.expires_at ? String(entry.expires_at).slice(0, 10) : "",
  };
}

export function grantViews(response) {
  const list = Array.isArray(response && response.grants) ? response.grants : [];
  return list.map(grantView);
}

/** A principal string in words, given the roster and the agents. */
export function principalLabel(principal, members, agents) {
  const text = String(principal || "");
  const [kind, id] = [text.slice(0, text.indexOf(":")), text.slice(text.indexOf(":") + 1)];
  if (kind === "member") {
    const found = (members || []).find((m) => m.user_id === id || m.userId === id);
    return found ? `${found.name || id}` : `member ${id}`;
  }
  if (kind === "agent") {
    const found = (agents || []).find((a) => a.id === id);
    return found ? `${found.name} (agent)` : `agent ${id}`;
  }
  if (kind === "role") return `everyone with the ${id} role`;
  return text;
}

export function accessErrorMessage(error, what) {
  if (error && error.status === 401) return "You are signed out — sign in again.";
  if (error && error.status === 403) return `You can't ${what}.`;
  const detail = error && error.body && (error.body.detail || error.body.title);
  return `Could not ${what}: ${detail || (error && error.message) || String(error)}`;
}

/* ---------------------------------------------------------- painters */

function td(doc, text, label) {
  const cell = doc.createElement("td");
  cell.textContent = text;
  if (label) cell.setAttribute("data-label", label);
  return cell;
}

function btn(doc, text, attrs, danger) {
  const b = doc.createElement("button");
  b.type = "button";
  b.className = danger ? "btn-sm btn-danger" : "btn-sm";
  b.textContent = text;
  for (const [name, value] of Object.entries(attrs)) b.setAttribute(name, value);
  return b;
}

/** A row's selection box, named for what it selects. */
function selectCell(doc, attr, id, checked, label) {
  const cell = doc.createElement("td");
  cell.className = "cell-select";
  const box = doc.createElement("input");
  box.type = "checkbox";
  box.checked = Boolean(checked);
  box.setAttribute(attr, id);
  box.setAttribute("aria-label", label);
  cell.appendChild(box);
  return cell;
}

function emptyRow(doc, container, text, span) {
  const tr = doc.createElement("tr");
  const cell = td(doc, text);
  cell.colSpan = span;
  tr.appendChild(cell);
  container.appendChild(tr);
}

export function renderAgentRows(
  container,
  views,
  doc = typeof document === "undefined" ? null : document,
  selected = new Set(),
) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const list = Array.isArray(views) ? views : [];
  if (!list.length) {
    emptyRow(doc, container, "No agent accounts yet. An agent reaches nothing until you grant it access.", 5);
    return container;
  }
  for (const view of list) {
    const tr = doc.createElement("tr");
    tr.appendChild(selectCell(doc, "data-agent-select", view.id, selected.has(view.id), `Select ${view.name}`));
    tr.appendChild(td(doc, view.name, "Name"));
    tr.appendChild(td(doc, view.description, "What it is for"));
    tr.appendChild(td(doc, view.disabled ? "Disabled" : "Active", "Status"));
    const actions = doc.createElement("td");
    actions.className = "cell-actions";
    actions.appendChild(btn(doc, "Keys", { "data-agent-keys": view.id, "data-name": view.name }));
    actions.appendChild(
      btn(doc, view.disabled ? "Enable" : "Disable", {
        "data-agent-toggle": view.id,
        "data-disabled": view.disabled ? "1" : "0",
      }),
    );
    actions.appendChild(btn(doc, "Delete", { "data-agent-delete": view.id, "data-name": view.name }, true));
    tr.appendChild(actions);
    container.appendChild(tr);
  }
  return container;
}

export function renderKeyRows(container, views, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const list = (Array.isArray(views) ? views : []).filter((v) => !v.revoked);
  if (!list.length) {
    emptyRow(doc, container, "No live keys.", 5);
    return container;
  }
  for (const view of list) {
    const tr = doc.createElement("tr");
    tr.appendChild(td(doc, view.name || "—", "Label"));
    tr.appendChild(td(doc, view.prefix, "Prefix"));
    tr.appendChild(td(doc, view.expires || "never", "Expires"));
    tr.appendChild(td(doc, view.lastUsed || "never", "Last used"));
    const actions = doc.createElement("td");
    actions.className = "cell-actions";
    actions.appendChild(btn(doc, "Rotate", { "data-key-rotate": view.prefix }));
    actions.appendChild(btn(doc, "Revoke", { "data-key-revoke": view.prefix }, true));
    tr.appendChild(actions);
    container.appendChild(tr);
  }
  return container;
}

export function renderGrantRows(
  container,
  views,
  members,
  agents,
  doc = typeof document === "undefined" ? null : document,
  selected = new Set(),
  emptyText = "No grants. Everyone reads their own space and the shared one.",
) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const list = Array.isArray(views) ? views : [];
  if (!list.length) {
    emptyRow(doc, container, emptyText, 6);
    return container;
  }
  for (const view of list) {
    const tr = doc.createElement("tr");
    const who = principalLabel(view.principal, members, agents);
    tr.appendChild(
      selectCell(doc, "data-grant-select", view.id, selected.has(view.id), `Select: ${who}, ${view.space} · ${view.folder}`),
    );
    tr.appendChild(td(doc, who, "Who"));
    tr.appendChild(td(doc, `${view.space} · ${view.folder}`, "Where"));
    tr.appendChild(td(doc, view.kind === "*" ? "everything" : view.kind, "What"));
    tr.appendChild(
      td(doc, view.effect === "deny" ? "denied" : view.permission + (view.expires ? ` until ${view.expires}` : ""), "Access"),
    );
    const actions = doc.createElement("td");
    actions.className = "cell-actions";
    actions.appendChild(btn(doc, "Remove", { "data-grant-delete": view.id }, true));
    tr.appendChild(actions);
    container.appendChild(tr);
  }
  return container;
}

export function renderAuditRows(container, entries, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  const list = Array.isArray(entries) ? entries : [];
  for (const entry of list) {
    const tr = doc.createElement("tr");
    tr.appendChild(td(doc, String(entry.at || "").replace("T", " ").slice(0, 16), "When"));
    const actor = entry.actor || {};
    const behalf = entry.on_behalf_of ? ` for ${entry.on_behalf_of}` : "";
    tr.appendChild(td(doc, `${actor.kind || ""} ${actor.id || ""}${behalf}`, "Who"));
    tr.appendChild(td(doc, String(entry.action || ""), "Did"));
    tr.appendChild(td(doc, String(entry.resource || ""), "To"));
    tr.appendChild(td(doc, String(entry.credential || ""), "With"));
    container.appendChild(tr);
  }
  return container;
}

/** The "who can see this page" list. */
export function renderReaders(container, answer, members, agents, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const readers = Array.isArray(answer && answer.readers) ? answer.readers : [];
  for (const reader of readers) {
    const li = doc.createElement("li");
    const who = doc.createElement("span");
    who.textContent = principalLabel(reader.principal, members, agents);
    li.appendChild(who);
    const what = doc.createElement("span");
    what.className = "muted";
    const how = reader.effect === "deny" ? "denied" : String(reader.permission || "");
    const where = reader.folder ? ` (folder ${reader.folder})` : "";
    what.textContent = ` — ${how}${where}`;
    li.appendChild(what);
    if (reader.grant_id && answer.you_can_share) {
      li.appendChild(btn(doc, "Remove", { "data-grant-delete": String(reader.grant_id) }, true));
    }
    container.appendChild(li);
  }
  return container;
}

/** Puts a freshly minted key in its one element. */
export function revealAgentKey(node, minted) {
  if (node) node.textContent = (minted && minted.key) || "";
  return node;
}

export function clearAgentKey(node) {
  return revealAgentKey(node, null);
}
