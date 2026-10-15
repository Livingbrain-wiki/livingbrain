// The deploy build (`tools/stamp-assets.mjs`): every reference the browser
// follows names a content-hashed file, nothing is left pointing at a mutable
// path, `_headers` caches each kind as intended, and the CSP's inline-script
// hash still matches the built pages.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, posix } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { build, findRefs, hashedName, plan } from "../../tools/stamp-assets.mjs";

const APP = fileURLToPath(new URL("..", import.meta.url));
const OUT = mkdtempSync(join(tmpdir(), "lb-stamp-test-"));
const result = build(APP, OUT);
process.on("exit", () => rmSync(OUT, { recursive: true, force: true }));

const read = (p) => readFileSync(join(OUT, p), "utf8");
const sha = (data) => createHash("sha256").update(data).digest("hex");
const PAGES = ["index.html", "settings.html", "wiki.html", "brain.html"];
const resolveFrom = (file, spec) => posix.normalize(posix.join(posix.dirname(file), spec.split(/[?#]/)[0]));

function walk(dir, rel = "") {
  return readdirSync(join(dir, rel)).flatMap((name) => {
    const p = rel ? `${rel}/${name}` : name;
    return statSync(join(dir, p)).isDirectory() ? walk(dir, p) : [p];
  });
}

test("every script, stylesheet, icon and preload on a page is a hashed file that exists", () => {
  for (const page of PAGES) {
    const html = read(page);
    const refs = findRefs(page, html).filter((r) => !/^(https?:|#)/.test(r.spec));
    assert.ok(refs.length >= 5, page);
    for (const { spec } of refs) {
      if (spec === "manifest.webmanifest") continue;
      assert.match(spec, /^static\/assets\/.+\.[0-9a-f]{10}\.[a-z0-9]+$/, `${page}: ${spec}`);
      assert.ok(existsSync(join(OUT, spec)), `${page}: ${spec} exists`);
    }
    assert.match(html, /<script type="module" src="static\/assets\/app\.[0-9a-f]{10}\.js">/);
    assert.doesNotMatch(html, /(?:src|href)="assets\//, `${page} keeps no unhashed asset`);
  }
});

test("every module import and stylesheet url() in the hashed tree is itself hashed", () => {
  const hashed = walk(OUT).filter((p) => p.startsWith("static/"));
  assert.equal(hashed.length, result.names.size);
  let imports = 0;
  for (const file of hashed.filter((p) => /\.(js|css)$/.test(p))) {
    for (const { spec, kind } of findRefs(file, read(file))) {
      const target = resolveFrom(file, spec);
      if (!existsSync(join(OUT, target))) continue; // prose in a comment
      imports += kind === "import" ? 1 : 0;
      assert.match(target, /^static\/.+\.[0-9a-f]{10}\.[a-z0-9]+$/, `${file} → ${spec}`);
    }
    // Belt and braces: no static import line names an unhashed sibling.
    for (const [, spec] of read(file).matchAll(/^\s*(?:import|export)\s[^;"'`]*?\bfrom\s*"([^"]+)"/gm)) {
      assert.match(spec, /\.[0-9a-f]{10}\.(js|json)$/, `${file} imports ${spec}`);
    }
  }
  assert.ok(imports > 30, `imports seen: ${imports}`);
  // The import that went stale in the field.
  assert.match(read(result.names.get("assets/app.js")), /from "\.\/github\.[0-9a-f]{10}\.js"/);
});

test("a hashed name is the hash of the bytes served under it", () => {
  for (const [source, name] of result.names) {
    const hash = /\.([0-9a-f]{10})\.[a-z0-9]+$/.exec(name)[1];
    assert.equal(name, hashedName(source, hash));
    // An import cycle's members (app.js ⇄ wiki.js) share a group hash
    // instead; the fixture test below covers how that one is built.
    if (result.cyclic.has(source)) continue;
    assert.equal(sha(readFileSync(join(OUT, name))).slice(0, 10), hash, source);
  }
  assert.ok(result.cyclic.has("assets/app.js") && result.cyclic.has("assets/wiki.js"));
  assert.ok(!result.cyclic.has("assets/github.js"));
});

test("the built module graph loads: the hashed app.js imports cleanly in Node", () => {
  const entry = pathToFileURL(join(OUT, result.names.get("assets/app.js"))).href;
  const run = spawnSync(process.execPath, ["--input-type=module", "-e", `await import(${JSON.stringify(entry)})`], {
    encoding: "utf8",
  });
  assert.equal(run.status, 0, run.stderr);
});

test("the manifest's icons are hashed; the originals stay at their old paths", () => {
  for (const [, src] of read("manifest.webmanifest").matchAll(/"src"\s*:\s*"([^"]+)"/g)) {
    assert.match(src, /^static\/assets\//);
    assert.ok(existsSync(join(OUT, src)), src);
  }
  // The Worker's device pages link these by name (device_pages.rs).
  for (const p of ["assets/app.css", "assets/device.css", "assets/favicon.svg", "assets/app.js"]) {
    assert.equal(read(p), readFileSync(join(APP, p), "utf8"), p);
  }
  assert.ok(!existsSync(join(OUT, "tests")), "tests are not deployed");
  assert.ok(!existsSync(join(OUT, "README.md")));
});

test("the built worker is versioned by the build and precaches the shell, all hashed", () => {
  const sw = read("sw.js");
  const version = /const VERSION = "([0-9a-f]{12})";/.exec(sw);
  assert.ok(version, "VERSION is the build hash");
  assert.equal(version[1], result.version);
  assert.doesNotMatch(sw, /const VERSION = "dev"/);
  const precache = JSON.parse(/const PRECACHE = (\[[^\]]*\]);/.exec(sw)[1]);
  assert.deepEqual(precache, result.precache);
  for (const page of PAGES) assert.ok(precache.includes(`./${page}`), page);
  for (const entry of precache.filter((e) => e !== "./")) {
    assert.ok(existsSync(join(OUT, entry)), entry);
    if (entry.endsWith(".html") || entry.endsWith(".webmanifest")) continue;
    assert.match(entry, /^\.\/static\/assets\/.+\.[0-9a-f]{10}\./, entry);
  }
  // Every module the pages reach, found by walking the graph — not a list.
  for (const module of ["app.js", "github.js", "update.js", "wiki.js", "shell.js", "nav.js", "brain-render.js"]) {
    assert.ok(precache.includes(`./${result.names.get(`assets/${module}`)}`), module);
  }
  assert.ok(precache.includes(`./${result.names.get("assets/providers.json")}`), "JSON modules too");
  assert.ok(!precache.includes(`./${result.names.get("assets/device.css")}`), "nothing no page loads");
});

test("the CSP's inline-script hash matches every built page, and only refs changed", () => {
  const headers = read("_headers");
  const pinned = /'sha256-([^']+)'/.exec(headers)[1];
  for (const page of PAGES) {
    const built = read(page);
    const inline = /<script>([\s\S]*?)<\/script>/.exec(built)[1];
    assert.equal(createHash("sha256").update(inline).digest("base64"), pinned, page);
    // Blank out the rewritten attribute values: the rest is byte-identical.
    const blank = (html) => html.replace(/((?:src|href)=")(?:static\/)?assets\/[^"]+"/g, '$1"');
    assert.equal(blank(built), blank(readFileSync(join(APP, page), "utf8")), page);
  }
});

test("_headers: pages revalidate, hashed files are immutable, originals revalidate", () => {
  const headers = readFileSync(join(APP, "_headers"), "utf8");
  const rule = (path) => {
    const at = headers.indexOf(`\n${path}\n`);
    assert.ok(at >= 0, `rule for ${path}`);
    return headers.slice(at + path.length + 2).split(/\n(?! )/)[0];
  };
  for (const path of ["/", "/index.html", "/settings", "/settings.html", "/wiki", "/wiki.html", "/brain", "/brain.html"]) {
    assert.match(rule(path), /Cache-Control: private, no-cache$/, path);
  }
  assert.match(rule("/static/*"), /Cache-Control: public, max-age=31536000, immutable$/);
  assert.match(rule("/assets/*"), /Cache-Control: public, no-cache$/);
  assert.match(rule("/sw.js"), /Cache-Control: no-cache$/);
  assert.match(rule("/manifest.webmanifest"), /Cache-Control: public, no-cache$/);
  assert.doesNotMatch(headers, /max-age=3600/, "no hour-long cache on a mutable path");
  // `/static/*` and `/assets/*` never overlap, so no file gets both values.
  assert.ok(!result.precache.some((e) => e.startsWith("./assets/")));
});

test("the build is deterministic", () => {
  const again = plan(APP);
  assert.equal(again.version, result.version);
  assert.deepEqual([...again.names], [...result.names]);
});

// --- the stamper on small fixtures -------------------------------------

function fixture(files) {
  const dir = mkdtempSync(join(tmpdir(), "lb-stamp-fixture-"));
  for (const [path, body] of Object.entries(files)) {
    mkdirSync(join(dir, posix.dirname(path)), { recursive: true });
    writeFileSync(join(dir, path), body);
  }
  return dir;
}

const SW_STUB = 'const VERSION = "dev";\nconst PRECACHE = [];\n';

test("a change deep in the graph renames every importer up to the page", () => {
  const base = {
    "index.html": '<link rel="stylesheet" href="assets/a.css"><script type="module" src="assets/a.js"></script>',
    "assets/a.js": 'import { b } from "./b.js";\nexport const a = b;\n',
    "assets/b.js": 'import { c } from "./lib/c.js";\nexport const b = c;\n',
    "assets/lib/c.js": "export const c = 1;\n",
    "assets/a.css": "body { background: url(img/x.svg); }\n",
    "assets/img/x.svg": "<svg/>",
    "sw.js": SW_STUB,
  };
  const one = plan(fixture(base));
  const two = plan(fixture({ ...base, "assets/lib/c.js": "export const c = 2;\n" }));
  for (const p of ["assets/a.js", "assets/b.js", "assets/lib/c.js"]) {
    assert.notEqual(one.names.get(p), two.names.get(p), p);
  }
  assert.equal(one.names.get("assets/a.css"), two.names.get("assets/a.css"), "unrelated file keeps its name");
  assert.notEqual(one.version, two.version);
  const b = one.files.get(one.names.get("assets/b.js")).toString();
  assert.match(b, /from "\.\/lib\/c\.[0-9a-f]{10}\.js"/);
  const css = one.files.get(one.names.get("assets/a.css")).toString();
  assert.match(css, /url\(img\/x\.[0-9a-f]{10}\.svg\)/);
  const sw = one.files.get("sw.js").toString();
  for (const p of ["assets/a.js", "assets/b.js", "assets/lib/c.js", "assets/a.css", "assets/img/x.svg"]) {
    assert.ok(sw.includes(`"./${one.names.get(p)}"`), p);
  }
});

test("an import cycle is hashed as a group and still rewritten", () => {
  const dir = fixture({
    "index.html": '<script type="module" src="assets/a.js"></script>',
    "assets/a.js": 'import { b } from "./b.js";\nexport const a = () => b;\n',
    "assets/b.js": 'import { a } from "./a.js";\nexport const b = () => a;\n',
    "sw.js": SW_STUB,
  });
  const out = plan(dir);
  const a = out.files.get(out.names.get("assets/a.js")).toString();
  const b = out.files.get(out.names.get("assets/b.js")).toString();
  assert.ok(a.includes(`"./${posix.basename(out.names.get("assets/b.js"))}"`));
  assert.ok(b.includes(`"./${posix.basename(out.names.get("assets/a.js"))}"`));
});

test("an import that resolves to nothing fails the build", () => {
  const dir = fixture({
    "index.html": '<script type="module" src="assets/a.js"></script>',
    "assets/a.js": 'import { x } from "./missing.js";\n',
    "sw.js": SW_STUB,
  });
  assert.throws(() => plan(dir), /missing\.js.*does not resolve/);
});

test("a worker without the build markers fails the build", () => {
  const dir = fixture({ "index.html": "", "sw.js": 'const CACHE = "lb-app-v1";\n' });
  assert.throws(() => plan(dir), /sw\.js: needs exactly one/);
});
