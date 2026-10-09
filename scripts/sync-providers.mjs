#!/usr/bin/env node
// Regenerates app/assets/providers.json from Colonizer's provider catalog.
//
// Living Brain connects to every LLM provider Colonizer does. The list lives
// upstream in Colonizer-dev/harness `web/src/providerCatalog.ts`; this script
// vendors it as plain data, which both the Rust server
// (`crates/livingbrain-models/src/catalog.rs`, through `include_str!`) and the
// static app (`app/assets/models.js`, through a JSON import) read. Nothing
// fetches it at runtime.
//
// Run it as `scripts/sync-providers` (a wrapper around this file).
//
// Usage:
//   scripts/sync-providers                 # latest main, rewrite the file
//   scripts/sync-providers --ref <sha|tag> # a pinned upstream revision
//   scripts/sync-providers --file <path>   # a local checkout's file (offline);
//                                          #   pass --ref too, to record it
//   scripts/sync-providers --check         # exit 1 if the vendored file would change
//   scripts/sync-providers --check --pinned
//                                          # the same, against the commit the file
//                                          #   records (what CI runs: deterministic)
//
// The upstream file is TypeScript, and it is parsed, never evaluated: the
// `PROVIDER_CATALOG` array literal is cut out, its object keys are quoted and
// numeric separators dropped, and the result must parse as JSON and pass the
// same checks `app/tests/providers.test.mjs` runs. Anything else is a hard
// failure, so a change in the upstream file's shape stops here rather than
// shipping a half-read catalog.
//
// The `builtin` entries (Anthropic and OpenAI, which Colonizer serves as its
// own defaults rather than listing) are Living Brain's and are kept as they
// are across a sync.

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const REPO = "Colonizer-dev/harness";
const PATH = "web/src/providerCatalog.ts";
const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = join(ROOT, "app/assets/providers.json");

const args = process.argv.slice(2);
const flag = (name) => {
  const at = args.indexOf(name);
  return at === -1 ? null : args[at + 1];
};
const check = args.includes("--check");
const localFile = flag("--file");
const pinned = args.includes("--pinned");
let ref = flag("--ref") || "main";
if (pinned) ref = JSON.parse(readFileSync(OUT, "utf8")).source.commit;

async function resolveCommit(wanted) {
  if (/^[0-9a-f]{40}$/.test(wanted)) return wanted;
  const res = await fetch(`https://api.github.com/repos/${REPO}/commits/${encodeURIComponent(wanted)}`, {
    headers: { Accept: "application/vnd.github+json" },
  });
  if (!res.ok) throw new Error(`could not resolve ${REPO}@${wanted}: HTTP ${res.status}`);
  return (await res.json()).sha;
}

async function source(commit) {
  if (localFile) return readFileSync(localFile, "utf8");
  const url = `https://raw.githubusercontent.com/${REPO}/${commit}/${PATH}`;
  const res = await fetch(url);
  if (!res.ok) throw new Error(`could not fetch ${url}: HTTP ${res.status}`);
  return res.text();
}

/** The `PROVIDER_CATALOG` array literal as data, without evaluating anything. */
export function parseCatalog(ts) {
  const start = ts.indexOf("PROVIDER_CATALOG");
  if (start === -1) throw new Error("PROVIDER_CATALOG not found upstream");
  const open = ts.indexOf("[", ts.indexOf("=", start));
  const end = open === -1 ? null : /\n\s*\];/.exec(ts.slice(open));
  if (open === -1 || !end) throw new Error("PROVIDER_CATALOG is not a plain array literal");
  const literal = ts.slice(open, open + end.index + end[0].length - 1);
  return JSON.parse(toJson(literal));
}

/**
 * A JS array literal of plain objects, rewritten as JSON. It walks the text
 * once and leaves string contents alone, so a name such as "A, b: c" is never
 * mistaken for a key. Outside strings it quotes bare keys, drops numeric
 * separators, line comments and trailing commas. Anything that is not data
 * (a call, a spread, a template string) survives into the output and makes
 * `JSON.parse` throw.
 */
function toJson(literal) {
  let out = "";
  let i = 0;
  while (i < literal.length) {
    const c = literal[i];
    if (c === '"') {
      let j = i + 1;
      while (j < literal.length && literal[j] !== '"') j += literal[j] === "\\" ? 2 : 1;
      out += literal.slice(i, j + 1);
      i = j + 1;
    } else if (c === "/" && literal[i + 1] === "/") {
      while (i < literal.length && literal[i] !== "\n") i += 1;
    } else if (/[A-Za-z_$]/.test(c)) {
      let j = i;
      while (j < literal.length && /[A-Za-z0-9_$]/.test(literal[j])) j += 1;
      const word = literal.slice(i, j);
      let k = j;
      while (/\s/.test(literal[k] || "")) k += 1;
      out += literal[k] === ":" ? `"${word}"` : word;
      i = j;
    } else if (/[0-9]/.test(c)) {
      let j = i;
      while (j < literal.length && /[0-9_.]/.test(literal[j])) j += 1;
      out += literal.slice(i, j).replace(/_/g, "");
      i = j;
    } else if (c === ",") {
      let k = i + 1;
      while (/\s/.test(literal[k] || "")) k += 1;
      if (literal[k] !== "]" && literal[k] !== "}") out += c;
      i += 1;
    } else {
      out += c;
      i += 1;
    }
  }
  return out;
  return JSON.parse(json);
}

const FIELDS = [
  "id",
  "name",
  "base_url",
  "auth",
  "wire",
  "site",
  "variables",
  "context_tokens",
  "models",
  "max_concurrent",
];

/** Throws on the first entry that is not a provider this app can use. */
export function validateEntries(entries, label = "providers") {
  if (!Array.isArray(entries) || entries.length === 0) throw new Error(`${label}: empty`);
  const seen = new Set();
  for (const entry of entries) {
    const where = `${label}: ${entry && entry.id}`;
    if (!entry || typeof entry !== "object") throw new Error(`${label}: not an object`);
    if (!/^[a-z0-9][a-z0-9-]*$/.test(entry.id || "")) throw new Error(`${where}: bad id`);
    if (seen.has(entry.id)) throw new Error(`${where}: duplicate id`);
    seen.add(entry.id);
    if (typeof entry.name !== "string" || !entry.name.trim()) throw new Error(`${where}: no name`);
    if (!/^https:\/\/[^\s/?#]+[^\s?#]*$/.test(entry.base_url || "")) {
      throw new Error(`${where}: base_url must be an https URL`);
    }
    if (!["bearer", "x-api-key"].includes(entry.auth)) throw new Error(`${where}: auth`);
    if (!["anthropic", "openai"].includes(entry.wire)) throw new Error(`${where}: wire`);
    if (typeof entry.site !== "string" || (entry.site && !/^https:\/\//.test(entry.site))) {
      throw new Error(`${where}: site must be https or empty`);
    }
    const placeholders = [...entry.base_url.matchAll(/\$\{(\w+)\}/g)].map((m) => m[1]).sort();
    const variables = (entry.variables || []).map((v) => v && v.name).sort();
    if (JSON.stringify(placeholders) !== JSON.stringify(variables)) {
      throw new Error(`${where}: variables do not match the base_url placeholders`);
    }
    for (const v of entry.variables || []) {
      if (!/^\w+$/.test(v.name) || typeof v.label !== "string" || typeof v.placeholder !== "string") {
        throw new Error(`${where}: malformed variable`);
      }
    }
    if (entry.models !== undefined) {
      if (!Array.isArray(entry.models) || !entry.models.every((m) => typeof m === "string" && m)) {
        throw new Error(`${where}: models must be strings`);
      }
    }
    for (const key of ["context_tokens", "max_concurrent"]) {
      if (entry[key] !== undefined && !(Number.isInteger(entry[key]) && entry[key] > 0)) {
        throw new Error(`${where}: ${key} must be a positive integer`);
      }
    }
    for (const key of Object.keys(entry)) {
      if (!FIELDS.includes(key)) throw new Error(`${where}: unexpected field ${key}`);
    }
  }
  return entries;
}

/** Keeps the fields Living Brain reads, in a stable order. */
function trim(entry) {
  const out = {};
  for (const key of FIELDS) if (entry[key] !== undefined) out[key] = entry[key];
  return out;
}

async function main() {
  const commit = localFile ? ref : await resolveCommit(ref);
  const upstream = parseCatalog(await source(commit)).map(trim);
  validateEntries(upstream, "upstream");

  const current = JSON.parse(readFileSync(OUT, "utf8"));
  validateEntries(current.builtin, "builtin");
  const clash = current.builtin.find((b) => upstream.some((u) => u.id === b.id));
  if (clash) throw new Error(`builtin id ${clash.id} now exists upstream; drop it from builtin`);

  const next = {
    _comment: [
      "GENERATED by scripts/sync-providers. Do not edit `providers` by hand: run the script.",
      `Upstream: https://github.com/${REPO}/blob/${commit}/${PATH}`,
      "Upstream data is adapted from cc-switch (https://github.com/farion1231/cc-switch), MIT;",
      "Colonizer and Living Brain do not endorse or vet the services listed.",
      "`builtin` is Living Brain's own and survives a sync.",
    ],
    source: { repo: REPO, path: PATH, commit },
    builtin: current.builtin,
    providers: upstream,
  };
  const text = `${JSON.stringify(next, null, 2)}\n`;
  const before = readFileSync(OUT, "utf8");
  if (check) {
    if (text !== before) {
      console.error(`app/assets/providers.json is out of date with ${REPO}@${commit}`);
      process.exit(1);
    }
    console.log(`up to date with ${REPO}@${commit} (${upstream.length} providers)`);
    return;
  }
  writeFileSync(OUT, text);
  console.log(`wrote ${upstream.length} providers from ${REPO}@${commit}`);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main().catch((error) => {
    console.error(`sync-providers: ${error.message}`);
    process.exit(1);
  });
}
