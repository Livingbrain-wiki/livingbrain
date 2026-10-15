#!/usr/bin/env python3
"""Create the shared "Living Brain" GitHub App and store its keys as Worker secrets.

A one-time OPERATOR step (docs/github.md, "The shared app"). It runs on the
operator's own machine, and no credential ever reaches a browser page, the
terminal, the Worker's routes or a file (unless `wrangler` fails, see below):

1. It serves one page on http://127.0.0.1:<port>/ with a form that posts the
   app manifest (manifest.json next to this file) to GitHub's "Create GitHub
   App" page for the organization. The manifest's `redirect_url` is this
   local server, the same pattern Probot's setup uses.
2. The operator clicks "Create GitHub App" on GitHub. GitHub sends the
   browser back to http://127.0.0.1:<port>/created?code=...&state=...
3. The script checks the state, exchanges the one-hour, single-use code
   (POST https://api.github.com/app-manifests/{code}/conversions; the code is
   the only credential that call needs), and pipes each value into
   `wrangler secret put NAME --env <env>` over stdin. Nothing is echoed.

If a `wrangler secret put` fails, the values not yet stored are written to a
0600 file in a fresh 0700 directory, and the script prints that path and the
commands to finish by hand. Delete that file once done.

Standard library only. Usage:

    python3 scripts/github-shared-app/setup.py --env production
    python3 scripts/github-shared-app/setup.py --env staging
    python3 scripts/github-shared-app/setup.py --env production --print-manifest
"""

from __future__ import annotations

import argparse
import hmac
import html
import json
import os
import re
import secrets
import shlex
import subprocess
import sys
import tempfile
import threading
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
VENTURE = REPO / "crates" / "livingbrain-venture"

ORIGINS = {
    "production": "https://app.livingbrain.wiki",
    "staging": "https://staging-api.livingbrain.wiki",
}
NAMES = {
    "production": "Living Brain",
    "staging": "Living Brain Staging",
}

# The Worker secrets, in the order they are set. Must match
# `SHARED_APP_KEYS` in crates/livingbrain-github-app/src/shared.rs.
SECRET_ORDER = [
    "GITHUB_APP_ID",
    "GITHUB_APP_SLUG",
    "GITHUB_APP_CLIENT_ID",
    "GITHUB_APP_PRIVATE_KEY",
    "GITHUB_WEBHOOK_SECRET",
    "GITHUB_APP_CLIENT_SECRET",
]

CODE = re.compile(r"^[A-Za-z0-9_-]{1,128}$")
ORG = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?$")


def manifest(base: str, name: str, redirect: str) -> dict:
    raw = (HERE / "manifest.json").read_text(encoding="utf-8")
    raw = raw.replace("{{BASE}}", base).replace("{{NAME}}", name).replace("{{REDIRECT}}", redirect)
    return json.loads(raw)


def convert(code: str) -> dict:
    """POST /app-manifests/{code}/conversions. Never prints the answer."""
    request = urllib.request.Request(
        f"https://api.github.com/app-manifests/{code}/conversions",
        method="POST",
        data=b"",
        headers={
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "livingbrain-shared-app-setup",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as error:
        raise SystemExit(f"GitHub refused the conversion ({error.code}). Start again.") from None


def secret_values(app: dict) -> list[tuple[str, str]]:
    values = {
        "GITHUB_APP_ID": str(app["id"]),
        "GITHUB_APP_SLUG": app["slug"],
        "GITHUB_APP_CLIENT_ID": app["client_id"],
        "GITHUB_APP_PRIVATE_KEY": app["pem"],
        "GITHUB_WEBHOOK_SECRET": app.get("webhook_secret") or "",
        "GITHUB_APP_CLIENT_SECRET": app["client_secret"],
    }
    missing = [name for name, value in values.items() if not value]
    if missing:
        raise SystemExit(f"GitHub's answer lacked {', '.join(missing)}. Nothing was stored.")
    return [(name, values[name]) for name in SECRET_ORDER]


def put_secrets(wrangler: list[str], env: str, pairs: list[tuple[str, str]]) -> list[str]:
    """Pipes each value into `wrangler secret put` over stdin. Answers the names set."""
    done: list[str] = []
    for index, (name, value) in enumerate(pairs):
        result = subprocess.run(
            [*wrangler, "secret", "put", name, "--env", env],
            input=value,
            text=True,
            cwd=VENTURE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            check=False,
        )
        if result.returncode != 0:
            rescue(env, pairs[index:], result.stderr)
            return done
        done.append(name)
        print(f"  set {name}")
    return done


def rescue(env: str, pairs: list[tuple[str, str]], stderr: str) -> None:
    """wrangler failed: keep what is left in a private file, never on screen."""
    folder = Path(tempfile.mkdtemp(prefix="livingbrain-github-app-"))
    os.chmod(folder, 0o700)
    path = folder / "secrets.json"
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(dict(pairs), handle, indent=2)
    # wrangler's own message names no secret value; show its last line only.
    last = (stderr or "").strip().splitlines()[-1:] or ["(no message)"]
    print(f"\nwrangler failed: {last[0]}", file=sys.stderr)
    print(f"The values not yet stored are in {path} (mode 0600).", file=sys.stderr)
    print("Finish by hand from crates/livingbrain-venture, one per key, then delete the file:", file=sys.stderr)
    for name, _ in pairs:
        print(
            f"  python3 -c \"import json,sys; sys.stdout.write(json.load(open('{path}'))['{name}'])\""
            f" | npx wrangler secret put {name} --env {env}",
            file=sys.stderr,
        )
    print(f"  rm -rf {folder}", file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--env", choices=sorted(ORIGINS), required=True)
    parser.add_argument("--org", default="Livingbrain-wiki", help="the GitHub organization that owns the app")
    parser.add_argument("--base", help="the web app's origin (default: the environment's)")
    parser.add_argument("--name", help="the app's name on GitHub (default: by environment)")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--wrangler", default="npx wrangler", help="how to run wrangler")
    parser.add_argument("--print-manifest", action="store_true", help="print the manifest and exit")
    args = parser.parse_args()

    if not ORG.match(args.org):
        parser.error("--org must be a GitHub organization login")
    base = (args.base or ORIGINS[args.env]).rstrip("/")
    if not base.startswith("https://"):
        parser.error("--base must be an https:// origin")
    name = args.name or NAMES[args.env]
    redirect = f"http://127.0.0.1:{args.port}/created"
    the_manifest = manifest(base, name, redirect)
    if args.print_manifest:
        print(json.dumps(the_manifest, indent=2))
        return 0

    wrangler = shlex.split(args.wrangler)
    check = subprocess.run([*wrangler, "whoami"], cwd=VENTURE, capture_output=True, text=True, check=False)
    if check.returncode != 0 or "not authenticated" in (check.stdout + check.stderr).lower():
        print("wrangler is not signed in (`npx wrangler login`), so the keys could not be stored. Stopping.")
        return 1

    state = secrets.token_urlsafe(32)
    action = f"https://github.com/organizations/{args.org}/settings/apps/new?state={state}"
    page = f"""<!doctype html><meta charset="utf-8"><title>Create the Living Brain GitHub App</title>
<body style="font-family:system-ui;max-width:40em;margin:3em auto">
<h1>Create the shared GitHub App</h1>
<p>Environment <b>{html.escape(args.env)}</b>, organization <b>{html.escape(args.org)}</b>,
origin <b>{html.escape(base)}</b>. GitHub opens its "Create GitHub App" page with
the name, the read-only permissions, the webhook and the callbacks filled in.</p>
<form method="post" action="{html.escape(action)}">
<input type="hidden" name="manifest" value="{html.escape(json.dumps(the_manifest))}">
<button type="submit">Continue to GitHub</button>
</form></body>"""

    result: dict = {}
    finished = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args) -> None:  # the query carries the code: never log it
            return

        def reply(self, status: int, body: str, kind: str = "text/plain; charset=utf-8") -> None:
            data = body.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self) -> None:  # noqa: N802 (the stdlib's name)
            url = urllib.parse.urlsplit(self.path)
            if url.path == "/":
                self.reply(200, page, "text/html; charset=utf-8")
                return
            if url.path != "/created" or finished.is_set():
                self.reply(404, "Not here.")
                return
            query = urllib.parse.parse_qs(url.query)
            code = (query.get("code") or [""])[0]
            given = (query.get("state") or [""])[0]
            if not hmac.compare_digest(given.encode(), state.encode()) or not CODE.match(code):
                self.reply(400, "The state or code did not match. Start the script again.")
                return
            finished.set()
            try:
                app = convert(code)
                pairs = secret_values(app)
            except SystemExit as error:
                result["error"] = str(error)
                self.reply(502, str(error))
                return
            print(f"\nGitHub created app {app['slug']} (id {app['id']}). Storing its keys for --env {args.env}:")
            done = put_secrets(wrangler, args.env, pairs)
            result.update(slug=app["slug"], id=app["id"], done=done)
            if len(done) == len(pairs):
                self.reply(
                    200,
                    f"Created {app['slug']} (app id {app['id']}) and stored {len(done)} Worker secrets "
                    f"for {args.env}. No key was shown here. You can close this tab.",
                )
            else:
                self.reply(500, "Some keys could not be stored. See the terminal.")

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    print(f"Open http://127.0.0.1:{args.port}/ in the browser where you are signed in to GitHub")
    print(f"as an owner of {args.org}. Waiting for GitHub (Ctrl-C to stop)...")
    try:
        while not finished.wait(0.5):
            pass
        # Let the handler finish its reply.
        while "done" not in result and "error" not in result:
            finished.wait(0.2)
    except KeyboardInterrupt:
        print("\nStopped. Nothing was created on this side.")
        return 1
    finally:
        server.shutdown()

    if "error" in result:
        print(result["error"], file=sys.stderr)
        return 1
    print(
        f"\nDone: {result['slug']} (id {result['id']}). Each `wrangler secret put` already deployed a new"
        "\nWorker version with the secret. Check it in Settings -> GitHub: the Install button shows."
        f"\nThe app's page: https://github.com/organizations/{args.org}/settings/apps/{result['slug']}"
    )
    return 0 if len(result["done"]) == len(SECRET_ORDER) else 1


if __name__ == "__main__":
    sys.exit(main())
