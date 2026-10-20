# `livingbrain view`: the 3D brain, locally

`livingbrain view` opens the brain map (the 3D graph at `app.livingbrain.wiki/brain`)
in your browser, served from your own machine.

```sh
livingbrain view                 # serve the map and open the browser
livingbrain view --no-open       # print the URL instead
livingbrain view --workspace ws  # another workspace's brain (see below)
```

Ctrl-C stops it. Nothing is written to disk.

## The same front end

The page is the web app's own files: `app/brain.html` and `app/assets/`. A build
script (`crates/livingbrain-cli/build.rs`) embeds them into the binary, so the
site and the CLI run the same code. Nothing is copied into a fork. The one
change made at serve time is the page's `lb-api` meta tag, which points the
map's API calls back at the local server, under the run's token. The page
ships that tag empty, and the test `the_embedded_app_is_the_web_app_and_its_imports_all_resolve`
checks that every module the app imports is embedded.

## How it works

```
browser ──► 127.0.0.1:<port>/<token>/…  ──► livingbrain view ──► API (/v1/…)
            (the page, its assets,         (adds the stored
             and the map's reads)           credential here)
```

- **Loopback only, random port.** The server binds `127.0.0.1` on port 0, so
  the OS picks a free one. It is not reachable from the network.
- **Per-run path token.** A 32-byte random value, hex-encoded, is the first
  path segment: `http://127.0.0.1:<port>/<token>/`. A new token every run.
  The token is read from `/dev/urandom` on Unix. The Windows fallback mixes
  the standard library's `RandomState` with the clock.
- **The credential stays here.** The CLI's stored token (the keychain, or
  `LIVINGBRAIN_TOKEN`) is attached to each forwarded request on the server
  side. It is not in the page, the URL, the output or the logs.
- **Only the map's reads are forwarded.** `GET` only, and only these paths and
  query keys:

  | Path | Query keys |
  | :--- | :--- |
  | `/v1/graph` | `project`, `kind`, `limit` |
  | `/v1/graph/activity` | `since` |
  | `/v1/pages/{slug}` | none |
  | `/v1/search` | `q`, `project`, `limit` |
  | `/v1/workspaces/me` | none |

  Any other path, or any other query key, is a `404`. A value with a character
  outside what the page's own URL encoding produces is refused too.
- **The app's other pages redirect.** `wiki.html`, `settings.html` and the
  rest are not served here. A link to one `302`s to the same page on the
  hosted app, so the map's "Open the page" link still works. The redirect
  carries no token.

## Security

Every request goes through these checks, in this order:

1. **`Host` must be `127.0.0.1:<port>` or `localhost:<port>`.** A page on
   another site that rebinds its DNS to loopback sends its own host name and is
   refused with `403`. This is the DNS-rebinding guard.
2. **The path must start with the run token**, compared in constant time.
   Anything else, including a missing or wrong token, is a plain `404`, so an
   answer does not show whether a token was close.
3. **Method must be `GET`.** Anything else is a `405`.
4. **No CORS header** on any response, so no other site can read one.

Every response also sends `Cache-Control: no-store`,
`Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff`,
`X-Frame-Options: DENY` and `Cross-Origin-Resource-Policy: same-origin`. The
page carries the site's own Content-Security-Policy (`app/_headers`), with the
hash of its one inline script. Up to 32 connections are served at once.

**One residual.** On macOS, Linux and Windows the browser is started with the
URL as an argument (`open`, `xdg-open`, `cmd /C start`). For the instant that
process runs, the URL, and so the run token, is visible in the process list to
the same user. `--no-open` avoids that. On a shared machine, another user may be able to
read a process list, so use `--no-open` there.

## Workspaces

`--workspace <id>` shows that workspace's brain. Its token comes from
`LIVINGBRAIN_TOKEN_<WORKSPACE>` (the id upper-cased, other characters as `_`),
or from the Mac app's keychain item for that workspace. Without it, the view
uses the default token from `livingbrain login`.

## Not built yet

- **The offline cache.** Issue #35 (the local cache) is not built, so `view`
  needs the network and `--offline` does not exist. The "opens in under a
  second on a warm cache" criterion waits on #35.
- **`--export png|svg`.** Not built.
- **A `--project` flag.** The map has its own project filter; use that.

## Troubleshooting

- **"not logged in — run `livingbrain login`".** No token for this run. Log in,
  or set `LIVINGBRAIN_TOKEN`.
- **The map says "Sign in to see your brain map."** The stored token has
  expired or been revoked. The API answered `401`, and the view says so. Run
  `livingbrain login`.
- **"could not open a browser".** The URL is still printed. Open it yourself.
- **A link opens a hosted page.** That is the redirect described above; the
  page itself is only the brain map.
