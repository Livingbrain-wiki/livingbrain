// Uploading notes from the Imports page (`assets/imports.js`).
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import { droppedFiles, importPath, importRequest, importSkipReason, importSummary } from "../assets/imports.js";

test("a path is folder-relative, forward-slashed, with no dot segments", () => {
  assert.equal(importPath("/vault/notes/one.md"), "vault/notes/one.md");
  assert.equal(importPath("a\\b\\c.md"), "a/b/c.md");
  assert.equal(importPath("../../etc/./x.md"), "etc/x.md");
  assert.equal(importPath(""), "");
});

test("only Markdown and text up to 1 MiB, never under a hidden folder", () => {
  assert.equal(importSkipReason("notes/one.md", 10), "");
  assert.equal(importSkipReason("notes/One.MARKDOWN", 10), "");
  assert.equal(importSkipReason("a.txt", 1024 * 1024), "");
  assert.equal(importSkipReason("a.txt", 1024 * 1024 + 1), "over 1 MB");
  assert.equal(importSkipReason("photo.png", 10), "not Markdown or text");
  assert.equal(importSkipReason("notes.d/README", 10), "not Markdown or text");
  assert.equal(importSkipReason(".obsidian/workspace.md", 10), "hidden");
  assert.equal(importSkipReason("", 10), "no name");
});

test("the request files one source into personal or shared memory", () => {
  assert.deepEqual(importRequest({ path: "one.md", body: "# One" }), {
    method: "POST",
    path: "/v1/pages/sources",
    body: { kind: "import", path: "one.md", body: "# One", scope: "personal" },
  });
  const shared = importRequest({ path: "one.md", body: "x", scope: "shared", project: " EarthOS.world " });
  assert.equal(shared.body.scope, "shared");
  assert.equal(shared.body.project, "EarthOS.world");
  assert.equal(importRequest({ path: "a.md", body: "", scope: "everyone" }).body.scope, "personal");
});

test("the summary counts as it goes and when it is done", () => {
  assert.equal(importSummary({ total: 4, created: 1, failed: 1 }), "Importing 2 of 4… · 1 failed");
  assert.equal(importSummary({ total: 3, created: 2, unchanged: 1, skipped: 2, done: true }), "2 imported · 1 already here · 2 skipped");
  assert.equal(importSummary({ done: true }), "Nothing to import.");
});

test("a dropped folder is walked, hidden folders are not entered", async () => {
  const fileEntry = (fullPath) => ({
    isFile: true,
    name: fullPath.split("/").pop(),
    fullPath,
    file: (ok) => ok({ name: fullPath.split("/").pop(), size: 1 }),
  });
  const dir = (name, fullPath, children) => ({
    isFile: false,
    isDirectory: true,
    name,
    fullPath,
    createReader() {
      let given = false;
      return { readEntries: (ok) => { const batch = given ? [] : children; given = true; ok(batch); } };
    },
  });
  const vault = dir("vault", "/vault", [
    fileEntry("/vault/one.md"),
    dir(".obsidian", "/vault/.obsidian", [fileEntry("/vault/.obsidian/app.md")]),
    dir("sub", "/vault/sub", [fileEntry("/vault/sub/two.md")]),
  ]);
  const items = [
    { kind: "file", webkitGetAsEntry: () => vault },
    { kind: "file", getAsFile: () => ({ name: "loose.txt", size: 1 }) },
    { kind: "string" },
  ];
  const paths = (await droppedFiles(items)).map((entry) => entry.path);
  assert.deepEqual(paths, ["vault/one.md", "vault/sub/two.md", "loose.txt"]);
});

test("the Imports view has the drop zone, both pickers and the scope choice", () => {
  const html = readFileSync(new URL("../wiki.html", import.meta.url), "utf8");
  for (const id of ["imports-upload", "imports-drop", "imports-files", "imports-folder", "imports-project", "imports-progress"]) {
    assert.match(html, new RegExp(`id="${id}"`), id);
  }
  assert.match(html, /name="imports-scope" value="personal" checked/);
  assert.match(html, /name="imports-scope" value="shared"/);
});
