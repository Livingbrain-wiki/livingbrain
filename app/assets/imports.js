// Uploading notes from the Imports page: the pure half.
//
// Which dropped or chosen files are imported (the CLI's rule: Markdown and
// plain text, at most the redactor's 1 MiB, nothing under a hidden folder such
// as `.obsidian` or `.git`), the path each one is filed under, and the request
// that files it. The server redacts every body and path again regardless.

/** The extensions `livingbrain import` takes, lowercased. */
export const IMPORT_EXTENSIONS = Object.freeze(["md", "markdown", "txt"]);

/** The largest file imported, in bytes: the redactor's input limit. */
export const IMPORT_MAX_BYTES = 1024 * 1024;

/** The longest path the server files, in bytes. */
const MAX_PATH_BYTES = 1024;

/** How many uploads run at once. */
export const IMPORT_CONCURRENCY = 3;

/** The path a file is filed under: its folder-relative path when it came in
 * a folder, else its name. Slashes are forward, with no leading one and no
 * `.` or `..` segments. "" when nothing usable is left. */
export function importPath(name) {
  const segments = String(name == null ? "" : name)
    .replace(/\\/g, "/")
    .split("/")
    .filter((segment) => segment && segment !== "." && segment !== "..");
  return segments.join("/");
}

/** Why a file is skipped, in words, or "" when it is imported. */
export function importSkipReason(path, size) {
  if (!path) return "no name";
  if (path.split("/").some((segment) => segment.startsWith("."))) return "hidden";
  const dot = path.lastIndexOf(".");
  const ext = dot > path.lastIndexOf("/") ? path.slice(dot + 1).toLowerCase() : "";
  if (!IMPORT_EXTENSIONS.includes(ext)) return "not Markdown or text";
  if (new TextEncoder().encode(path).length > MAX_PATH_BYTES) return "path too long";
  if (Number(size) > IMPORT_MAX_BYTES) return "over 1 MB";
  return "";
}

/** `POST /v1/pages/sources`: one file into `personal` or `shared` memory,
 * optionally filed under a project. */
export function importRequest({ path, body, scope = "personal", project = "" }) {
  const payload = { kind: "import", path, body: String(body == null ? "" : body), scope: scope === "shared" ? "shared" : "personal" };
  const trimmed = String(project || "").trim();
  if (trimmed) payload.project = trimmed;
  return { method: "POST", path: "/v1/pages/sources", body: payload };
}

/** The one line the upload reports as it goes and when it is done. */
export function importSummary({ total = 0, created = 0, unchanged = 0, failed = 0, skipped = 0, done = false }) {
  const finished = created + unchanged + failed;
  const parts = [];
  if (!done && total) parts.push(`Importing ${finished} of ${total}…`);
  if (done) parts.push(`${created} imported`);
  if (done && unchanged) parts.push(`${unchanged} already here`);
  if (failed) parts.push(`${failed} failed`);
  if (skipped) parts.push(`${skipped} skipped`);
  if (done && !total && !skipped) return "Nothing to import.";
  return parts.join(" · ");
}

/**
 * The files under dropped items, with their folder-relative paths: a folder
 * is walked (`webkitGetAsEntry`), a plain file is itself. Hidden folders are
 * not entered. Answers `[{ file, path }]`.
 */
export async function droppedFiles(items) {
  const out = [];
  const readAll = (reader) =>
    new Promise((resolve) => {
      const entries = [];
      const next = () =>
        reader.readEntries(
          (batch) => {
            if (!batch.length) return resolve(entries);
            entries.push(...batch);
            next();
          },
          () => resolve(entries),
        );
      next();
    });
  const walk = async (entry) => {
    if (!entry || entry.name.startsWith(".")) return;
    if (entry.isFile) {
      const file = await new Promise((resolve) => entry.file(resolve, () => resolve(null)));
      if (file) out.push({ file, path: importPath(entry.fullPath || file.name) });
    } else if (entry.isDirectory) {
      for (const child of await readAll(entry.createReader())) await walk(child);
    }
  };
  const entries = [];
  const loose = [];
  for (const item of Array.from(items || [])) {
    if (item.kind !== "file") continue;
    const entry = typeof item.webkitGetAsEntry === "function" ? item.webkitGetAsEntry() : null;
    if (entry) entries.push(entry);
    else {
      const file = item.getAsFile();
      if (file) loose.push({ file, path: importPath(file.name) });
    }
  }
  for (const entry of entries) await walk(entry);
  return out.concat(loose);
}
