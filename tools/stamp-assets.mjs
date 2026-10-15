#!/usr/bin/env node
// Content-hashes the web app's assets into a build directory the Worker serves.
//
//   node tools/stamp-assets.mjs                  # app/ → build/app/
//   node tools/stamp-assets.mjs --check          # build into a temp dir, verify, discard
//   node tools/stamp-assets.mjs --src DIR --out DIR
//
// Why: `app/` has no bundler, and a deploy used to leave browsers on old code —
// `assets/*` was cached for an hour, and an ES module's imports are cached on
// their own, so a fresh `settings.html` could run last deploy's `github.js`.
// Every cacheable file under `assets/` now also exists under `static/` with a
// hash of its content in its name (`static/assets/app.3f9c0e12ab.js`), and
// every reference to it — `<script src>`, `<link href>`, a module's `import`,
// a stylesheet's `url()`, the manifest's icons and the service worker's
// precache list — is rewritten to that name. A hashed file never changes, so
// `_headers` caches `/static/*` for a year as immutable; the pages are
// revalidated on every load, so a new deploy is picked up on the next one.
//
// A module's name hashes its *rewritten* text, which already names its
// imports by their hashes, so a change anywhere below a module renames it and
// everything that imports it (a cycle is hashed as one group). `app/` itself
// is never modified, and the unhashed originals are still copied to the build
// at their old paths: the Worker's device pages link `/assets/app.css` by name,
// and a page loaded before the deploy keeps working.
//
// wrangler.toml's `[build] command` runs this after `worker-build`, so
// `wrangler deploy` (CI and ~/lb-deploy.sh) and `wrangler deploy --dry-run`
// build it with no extra step. Node's standard library only, like the app.

import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, posix, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Where hashed copies live in the build: `assets/x.js` → `static/assets/x.<hash>.js`. */
export const STATIC_PREFIX = "static/";

/** Source files that are never deployed (also listed in `app/.assetsignore`). */
const SKIP = new Set(["tests", "README.md"]);

/** What gets a hashed copy: everything cacheable under `assets/`. */
const HASHABLE_EXT = new Set([
  ".js", ".mjs", ".css", ".json", ".svg", ".png", ".jpg", ".jpeg", ".gif",
  ".webp", ".avif", ".ico", ".woff", ".woff2", ".ttf",
]);

const HASH_LEN = 10;

/** The two lines of `sw.js` the build replaces; both must be present exactly once. */
export const SW_VERSION_LINE = 'const VERSION = "dev";';
export const SW_PRECACHE_RE = /const PRECACHE = \[[^\]]*\];/;

const sha256 = (data) => createHash("sha256").update(data).digest("hex");

const extOf = (path) => {
  const base = posix.basename(path);
  const dot = base.lastIndexOf(".");
  return dot > 0 ? base.slice(dot) : "";
};

export function isHashable(path) {
  return path.startsWith("assets/") && HASHABLE_EXT.has(extOf(path));
}

/** `assets/fonts/x.woff2` + `abc` → `static/assets/fonts/x.abc.woff2`. */
export function hashedName(path, hash) {
  const ext = extOf(path);
  return `${STATIC_PREFIX}${path.slice(0, path.length - ext.length)}.${hash}${ext}`;
}

function walk(root, rel = "") {
  const out = [];
  for (const name of readdirSync(join(root, rel)).sort()) {
    const path = rel ? `${rel}/${name}` : name;
    if (!rel && SKIP.has(name)) continue;
    if (statSync(join(root, path)).isDirectory()) out.push(...walk(root, path));
    else out.push(path);
  }
  return out;
}

const isExternal = (spec) =>
  /^(?:[a-z][a-z0-9+.-]*:|\/\/|#)/i.test(spec) || spec.trim() === "";

/** Resolves `spec` (as written in `from`) to a root-relative path, or null. */
function resolveSpec(from, spec) {
  if (isExternal(spec)) return null;
  const bare = spec.split(/[?#]/)[0];
  if (!bare) return null;
  const joined = bare.startsWith("/") ? bare.slice(1) : posix.join(posix.dirname(from), bare);
  const norm = posix.normalize(joined);
  return norm.startsWith("../") ? null : norm;
}

/**
 * Every rewritable reference in a text file: `{ index, length, spec, kind }`,
 * where `index`/`length` cover the specifier text only.
 */
export function findRefs(path, text) {
  const refs = [];
  const push = (match, spec, kind) => {
    const index = match.index + match[0].lastIndexOf(spec);
    refs.push({ index, length: spec.length, spec, kind });
  };
  const ext = extOf(path);
  if (ext === ".js" || ext === ".mjs") {
    // `import x from "…"`, `export … from "…"`, `import "…"`, `import("…")`.
    const re = /(?:\bfrom\s*|\bimport\s*\(\s*|\bimport\s+)(["'])([^"'\n]+)\1/g;
    for (const m of text.matchAll(re)) push(m, m[2], "import");
  } else if (ext === ".css") {
    for (const m of text.matchAll(/url\(\s*(["']?)([^"')\s]+)\1\s*\)/g)) push(m, m[2], "url");
    for (const m of text.matchAll(/@import\s+(["'])([^"']+)\1/g)) push(m, m[2], "url");
  } else if (ext === ".html") {
    for (const tag of text.matchAll(/<(?:script|link)\b[^>]*>/gi)) {
      for (const attr of tag[0].matchAll(/\s(?:src|href)\s*=\s*"([^"]*)"/gi)) {
        const index = tag.index + attr.index + attr[0].lastIndexOf(attr[1]);
        refs.push({ index, length: attr[1].length, spec: attr[1], kind: "html" });
      }
    }
  } else if (ext === ".webmanifest") {
    for (const m of text.matchAll(/"src"\s*:\s*"([^"]+)"/g)) push(m, m[1], "manifest");
  }
  return refs;
}

/** A posix relative path from the file at `fromOut` to `toOut`, both build-root relative. */
function relativeRef(fromOut, toOut) {
  let rel = posix.relative(posix.dirname(fromOut), toOut);
  if (!rel.startsWith(".")) rel = `./${rel}`;
  return rel;
}

/** Rewrites `text` (living at `outPath` in the build) with `names` (source → hashed). */
function rewrite(srcPath, outPath, text, names) {
  const refs = findRefs(srcPath, text);
  let out = "";
  let last = 0;
  for (const ref of refs) {
    const target = resolveSpec(srcPath, ref.spec);
    if (!target || !names.has(target)) continue;
    let next = ref.spec.startsWith("/")
      ? `/${names.get(target)}`
      : relativeRef(outPath, names.get(target));
    // A page's `assets/x` reads `static/assets/x…`, not `./static/…`: keep the
    // spelling the source used.
    if (!ref.spec.startsWith("./") && !ref.spec.startsWith("/") && next.startsWith("./")) {
      next = next.slice(2);
    }
    const suffix = ref.spec.slice(ref.spec.split(/[?#]/)[0].length);
    out += text.slice(last, ref.index) + next + suffix;
    last = ref.index + ref.length;
  }
  return out + text.slice(last);
}

/** Tarjan's SCCs, emitted dependencies-first. */
function sccs(nodes, edges) {
  let counter = 0;
  const index = new Map();
  const low = new Map();
  const stack = [];
  const on = new Set();
  const out = [];
  const visit = (v) => {
    index.set(v, counter);
    low.set(v, counter);
    counter += 1;
    stack.push(v);
    on.add(v);
    for (const w of edges.get(v) || []) {
      if (!index.has(w)) {
        visit(w);
        low.set(v, Math.min(low.get(v), low.get(w)));
      } else if (on.has(w)) {
        low.set(v, Math.min(low.get(v), index.get(w)));
      }
    }
    if (low.get(v) === index.get(v)) {
      const group = [];
      let w;
      do {
        w = stack.pop();
        on.delete(w);
        group.push(w);
      } while (w !== v);
      out.push(group.sort());
    }
  };
  for (const v of nodes) if (!index.has(v)) visit(v);
  return out;
}

/**
 * Plans the build in memory. Returns `{ files, names, cyclic, precache, version }`:
 * `files` maps every build path to its bytes, `names` maps each hashed
 * source path to its build path. Throws on a reference that resolves to
 * nothing, so a typo fails the build rather than a browser.
 */
export function plan(srcDir) {
  const paths = walk(srcDir);
  const exists = new Set(paths);
  const raw = new Map(paths.map((p) => [p, readFileSync(join(srcDir, p))]));
  const text = (p) => raw.get(p).toString("utf8");
  const refBearing = (p) => [".js", ".mjs", ".css", ".html", ".webmanifest"].includes(extOf(p));

  // The reference graph, with every relative reference checked.
  const deps = new Map();
  for (const p of paths) {
    if (!refBearing(p) || p === "sw.js") continue;
    const targets = [];
    for (const ref of findRefs(p, text(p))) {
      const target = resolveSpec(p, ref.spec);
      if (!target) continue;
      if (!exists.has(target)) {
        // A page's `<a>`/`<link rel=manifest>` may name something the Worker
        // serves; a module import or a `url()` must be a file here.
        if (ref.kind === "import" || ref.kind === "url" || ref.kind === "manifest") {
          throw new Error(`${p}: "${ref.spec}" does not resolve to a file in the app`);
        }
        continue;
      }
      targets.push(target);
    }
    deps.set(p, targets);
  }

  // Hash the assets dependencies-first.
  const hashable = paths.filter(isHashable);
  const edges = new Map(hashable.map((p) => [p, (deps.get(p) || []).filter(isHashable)]));
  const names = new Map();
  const files = new Map();
  const cyclic = new Set();
  for (const group of sccs(hashable, edges)) {
    if (group.length === 1 && !edges.get(group[0]).includes(group[0])) {
      const [p] = group;
      const bytes = refBearing(p)
        ? Buffer.from(rewrite(p, STATIC_PREFIX + p, text(p), names), "utf8")
        : raw.get(p);
      const name = hashedName(p, sha256(bytes).slice(0, HASH_LEN));
      names.set(p, name);
      files.set(name, bytes);
    } else {
      // An import cycle: one hash for the group, from every member's source
      // and every name the group imports from outside it.
      const outside = [...new Set(group.flatMap((p) => edges.get(p)))]
        .filter((d) => !group.includes(d))
        .sort()
        .map((d) => names.get(d));
      const groupHash = sha256(
        JSON.stringify([group.map((p) => [p, sha256(raw.get(p))]), outside]),
      );
      for (const p of group) {
        cyclic.add(p);
        names.set(p, hashedName(p, sha256(`${groupHash}:${p}`).slice(0, HASH_LEN)));
      }
      for (const p of group) {
        files.set(names.get(p), Buffer.from(rewrite(p, STATIC_PREFIX + p, text(p), names), "utf8"));
      }
    }
  }

  // Everything else at its own path: the originals untouched, and the pages
  // and the manifest pointing at the hashed names.
  for (const p of paths) {
    if (p === "sw.js") continue;
    const rewritten =
      extOf(p) === ".html" || extOf(p) === ".webmanifest"
        ? Buffer.from(rewrite(p, p, text(p), names), "utf8")
        : raw.get(p);
    files.set(p, rewritten);
  }

  // The service worker precaches the pages and everything they load.
  const pages = paths.filter((p) => !p.includes("/") && extOf(p) === ".html");
  const reached = new Set();
  const visit = (p) => {
    for (const d of deps.get(p) || []) {
      if (isHashable(d) && !reached.has(d)) {
        reached.add(d);
        visit(d);
      }
    }
  };
  pages.forEach(visit);
  const precache = [
    "./",
    ...pages.map((p) => `./${p}`),
    ...(exists.has("manifest.webmanifest") ? ["./manifest.webmanifest"] : []),
    ...[...reached].sort().map((p) => `./${names.get(p)}`),
  ];

  // The build's version: every byte it serves except the worker itself.
  const version = sha256(
    [...files.keys()].sort().map((k) => `${k}\0${sha256(files.get(k))}`).join("\n"),
  ).slice(0, 12);

  if (exists.has("sw.js")) {
    const sw = text("sw.js");
    const count = (needle) => sw.split(needle).length - 1;
    if (count(SW_VERSION_LINE) !== 1 || (sw.match(new RegExp(SW_PRECACHE_RE, "g")) || []).length !== 1) {
      throw new Error(`sw.js: needs exactly one \`${SW_VERSION_LINE}\` and one \`const PRECACHE = [...];\``);
    }
    const built = sw
      .replace(SW_VERSION_LINE, `const VERSION = ${JSON.stringify(version)};`)
      .replace(SW_PRECACHE_RE, `const PRECACHE = ${JSON.stringify(precache, null, 2)};`);
    files.set("sw.js", Buffer.from(built, "utf8"));
  }

  return { files, names, cyclic, precache, version };
}

/** Writes a plan to `outDir`, replacing whatever was there. */
export function build(srcDir, outDir) {
  const result = plan(srcDir);
  rmSync(outDir, { recursive: true, force: true });
  for (const [path, bytes] of result.files) {
    const target = join(outDir, path);
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, bytes);
  }
  return result;
}

function main(argv) {
  const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");
  const arg = (flag, fallback) => {
    const at = argv.indexOf(flag);
    return at >= 0 && argv[at + 1] ? resolve(argv[at + 1]) : fallback;
  };
  const src = arg("--src", join(repo, "app"));
  const check = argv.includes("--check");
  const out = check
    ? mkdtempSync(join(tmpdir(), "lb-stamp-"))
    : arg("--out", join(repo, "build", "app"));
  if (!existsSync(src)) throw new Error(`no app at ${src}`);
  try {
    const { names, precache, version } = build(src, out);
    console.log(
      `stamp-assets: ${names.size} hashed assets, ${precache.length} precached, version ${version}` +
        (check ? " (check only)" : ` → ${out}`),
    );
  } finally {
    if (check) rmSync(out, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(`stamp-assets: ${error.message}`);
    process.exit(1);
  }
}
