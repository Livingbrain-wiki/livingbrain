// The settings request builder — the one place a settings change is described.
//
// `settingRequest(section, key, value)` is pure: it turns a section, a key and
// a value into the exact request that would go to the API, or into a marked
// "not wired yet" descriptor. `app.js` calls it and sends the result, and
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
  "proactivity",
  "tools",
  "automations",
  "skills",
  "colonizer",
];

/**
 * Which sections have a server route today.
 *
 * `models` is the only one: `PUT /v1/models/{role}` connects or replaces a
 * model connection for a role (`triage`, `main`, `research`), admin or owner
 * only. The other five are on the page because the issue asks for them and
 * because a section with no route must look *unavailable*, not broken — so
 * they render as disabled controls rather than as buttons that 404.
 */
const ENDPOINTS = {
  models: (key) => ({
    method: "PUT",
    path: `/v1/models/${encodeURIComponent(key)}`,
  }),
  proactivity: null,
  tools: null,
  automations: null,
  skills: null,
  colonizer: null,
};

/** The model roles the server accepts (`livingbrain-models` `ROLES`). */
export const MODEL_ROLES = ["triage", "main", "research"];

/** True when the section is one the page knows about. */
export function isSection(section) {
  return SECTIONS.includes(section);
}

/**
 * Describes one setting change.
 *
 * Returns:
 *   `{section, key, value, wired, method, path, body}` where `wired: false`
 *   means `method` and `path` are null and the caller must not send anything.
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

  const endpoint = ENDPOINTS[section];
  const body = { section, key: name, value };

  if (!endpoint) {
    return {
      section,
      key: name,
      value,
      wired: false,
      method: null,
      path: null,
      body,
    };
  }

  const route = endpoint(name);
  // The models handler reads its fields flat (`provider`, `base_url`,
  // `api_key`, `model`, `fallback_to_managed`), so they are spread at the top
  // level next to the audit triple. Serde ignores the extra keys it does not
  // know, and `section`/`key`/`value` are what the audit reads.
  const payload =
    section === "models" && value && typeof value === "object" && !Array.isArray(value)
      ? { ...value, section, key: name, value }
      : body;

  return {
    section,
    key: name,
    value,
    wired: true,
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
  if (!request.wired) {
    return `${name} has no server route yet — nothing was sent.`;
  }
  return ok ? `${name} saved.` : `${name} could not be saved.`;
}