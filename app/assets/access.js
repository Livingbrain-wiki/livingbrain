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

function emptyRow(doc, container, text, span) {
  const tr = doc.createElement("tr");
  const cell = td(doc, text);
  cell.colSpan = span;
  tr.appendChild(cell);
  container.appendChild(tr);
}

export function renderAgentRows(container, views, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const list = Array.isArray(views) ? views : [];
  if (!list.length) {
    emptyRow(doc, container, "No agent accounts yet. An agent reaches nothing until you grant it access.", 4);
    return container;
  }
  for (const view of list) {
    const tr = doc.createElement("tr");
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

export function renderGrantRows(container, views, members, agents, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const list = Array.isArray(views) ? views : [];
  if (!list.length) {
    emptyRow(doc, container, "No grants. Everyone reads their own space and the shared one.", 5);
    return container;
  }
  for (const view of list) {
    const tr = doc.createElement("tr");
    tr.appendChild(td(doc, principalLabel(view.principal, members, agents), "Who"));
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
