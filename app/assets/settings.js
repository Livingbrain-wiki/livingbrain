// The settings request builder — the one place a settings change is described.
//
// `settingRequest(section, key, value)` is pure: it turns a section, a key and
// a value into the exact request that would go to the API, and throws for a
// section it does not know. `app.js` calls it and sends the result, and
// nothing else in the app is allowed to mutate a setting: the pages have no
// other path to the API for settings.
//
// The body always carries `section`, `key` and `value` next to the change
// itself, so a server (or a proxy in front of it) can attribute every write to
// a named setting without parsing the URL — that is what makes the settings
// auditable, and it is why the shape lives here, in a pure function, where the
// tests can assert it.


/** The sections the settings page renders, in the order the page shows them. */
export const SECTIONS = [
  "models",
  "model-switch",
  "model-runner",
  "jev",
  "jev-test",
  "budgets",
];

/**
 * Which sections have a server route today.
 *
 * Every section here has a server route: `PUT /v1/models/{role}` connects or
 * replaces a model connection for a role (`triage`, `main`, `research`), admin
 * or owner only, and `PATCH /v1/settings/budgets` changes the workspace's
 * budgets and limits. A section without a route is not listed, so the page
 * never offers a control that cannot save.
 */
const ENDPOINTS = {
  // A connection object connects (or replaces) the role's model; `null`
  // disconnects it — `DELETE /v1/models/{role}` — so removing a model is a
  // settings change like any other and goes through the same choke point.
  models: (key, value) => ({
    method: value === null ? "DELETE" : "PUT",
    path: `/v1/models/${encodeURIComponent(key)}`,
  }),
  // Switches the model of a role's existing connection on the key already
  // stored for it (`PATCH /v1/models/{role}`, `{model}`): the top "Main
  // model" picker, which never asks for the key again.
  "model-switch": (key) => ({
    method: "PATCH",
    path: `/v1/models/${encodeURIComponent(key)}`,
  }),
  // The member's own device runner for a role — their Claude or ChatGPT
  // subscription, run by their own CLI on their own machine. Any member sets
  // their own; `null` turns it off for that role.
  "model-runner": (key, value) => ({
    method: value === null ? "DELETE" : "PUT",
    path: `/v1/models/mine/${encodeURIComponent(key)}`,
  }),
  // Jev, the coding-agents compiler's optional triage classifier (owner or
  // admin): `PUT /v1/models/jev` with only what changed (`enabled`,
  // `api_key`, `model`); key `key` with `null` forgets the workspace's own
  // TypeSafe key — `DELETE /v1/models/jev/key`.
  jev: (key, value) =>
    key === "key" && value === null
      ? { method: "DELETE", path: "/v1/models/jev/key" }
      : { method: "PUT", path: "/v1/models/jev" },
  // One cheap classification call (`POST /v1/models/jev/test`): changes
  // nothing, but goes through the same choke point so a key typed into the
  // sheet and tried is accounted for like a save.
  "jev-test": () => ({ method: "POST", path: "/v1/models/jev/test" }),
  // The workspace's budgets and limits (Settings → Usage & limits): one
  // `PATCH /v1/settings/budgets` carrying only what changed, owner or admin
  // only. The value is the patch itself (`{coding_agents, ask, spend}`),
  // spread beside the audit triple like the models sections.
  budgets: () => ({
    method: "PATCH",
    path: "/v1/settings/budgets",
  }),
};

/** The sections whose handler reads its fields flat, beside the audit triple. */
const FLAT_SECTIONS = ["models", "model-switch", "model-runner", "jev", "jev-test", "budgets"];

/** The model roles the server accepts (`livingbrain-models` `ROLES`). */
export const MODEL_ROLES = ["triage", "main", "research"];

/** True when the section is one the page knows about. */
export function isSection(section) {
  return SECTIONS.includes(section);
}

/**
 * Describes one setting change: `{section, key, value, method, path, body}`.
 *
 * Throws on an unknown section or an empty key, because a change that cannot
 * be named cannot be audited, and a silent no-op in the settings page is worse
 * than a visible error.
 */
export function settingRequest(section, key, value) {
  if (!isSection(section)) {
    throw new Error(`unknown settings section: ${String(section)}`);
  }
  const name = String(key == null ? "" : key).trim();
  if (!name) {
    throw new Error("a settings change needs a key");
  }

  const body = { section, key: name, value };

  const route = ENDPOINTS[section](name, value);
  // The models handler reads its fields flat (`provider`, `base_url`,
  // `api_key`, `model`, `fallback_to_managed`), so they are spread at the top
  // level next to the audit triple. Serde ignores the extra keys it does not
  // know, and `section`/`key`/`value` are what the audit reads.
  const payload =
    FLAT_SECTIONS.includes(section) && value && typeof value === "object" && !Array.isArray(value)
      ? { ...value, section, key: name, value }
      : body;

  return {
    section,
    key: name,
    value,
    method: route.method,
    path: route.path,
    body: payload,
  };
}

/**
 * The audit line a server would write for a change. Exported so the shape is
 * pinned by a test rather than only implied by the request body.
 */
export function auditRecord(request) {
  return {
    section: request.section,
    key: request.key,
    value: request.value,
    method: request.method,
    path: request.path,
  };
}

/** Human-readable one-liners for the notice area, kept here so they test too. */
export function describeOutcome(request, ok) {
  const name = `${request.section}.${request.key}`;
  if (request.method === "DELETE") {
    return ok ? `${name} removed.` : `${name} could not be removed.`;
  }
  return ok ? `${name} saved.` : `${name} could not be saved.`;
}
