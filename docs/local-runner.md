# The local runner: brain jobs on your own subscription

Living Brain can use a **Claude Pro/Max** or **ChatGPT Plus/Pro** subscription
as the model for the brain's jobs (compiling your coding-agent sessions,
answering Ask) — but only the way those subscriptions are allowed to be used:
inside the vendor's own client, on your own machine.

## The rule this is built around

Anthropic and OpenAI permit consumer subscriptions only inside their own
clients (Claude Code, Codex). So a subscription's credentials — the Claude.ai
OAuth token, the "Sign in with ChatGPT" token — are **never** sent to, stored
on, or used from the Living Brain server. Instead:

1. You choose "run my jobs on my device" (Settings → Models, the
   *Claude subscription* or *ChatGPT subscription* card).
2. When one of your jobs needs a model, the server queues it instead of
   calling an API. The job holds the same redacted prompt an API model would
   have been sent.
3. `livingbrain jobs run` (or the Mac app, which runs it for you) claims the
   job, runs **your** `claude` or `codex` with the prompt, and posts the text
   back. The server redacts and validates the answer again on arrival.

Only the job's owner can claim it; a claim is a lease, and a job whose runner
went away becomes claimable again when the lease lapses. Tokens the CLI
reports are recorded as usage at no cost to the workspace.

## What runs

The prompt (the job's user turn) goes on **stdin**. Each job runs in a fresh,
empty, private temp directory that is deleted afterwards, with the Living
Brain token removed from the child's environment. Verified against Claude
Code 2.1.300 and codex-cli 0.148.0:

```sh
# Claude Code — no tools, no MCP servers, no settings, no slash commands, one turn
claude -p --output-format json --tools "" --permission-mode dontAsk \
  --strict-mcp-config --setting-sources "" --disable-slash-commands \
  --no-session-persistence --max-turns 1 --system-prompt <instructions> \
  [--model <sonnet|opus|haiku|fable|full id>]

# Codex — the read-only sandbox, ephemeral, user config and rules ignored;
# stdin is the instructions, a blank line, then the prompt
codex exec --sandbox read-only --skip-git-repo-check --ephemeral \
  --ignore-user-config --ignore-rules --json --color never \
  -C <tmpdir> -o <tmpdir>/last-message.txt [-m <model>] -
```

`--setting-sources user` replaces `""` only for a job set up as "Claude Code
pointed at Z.AI's GLM Coding Plan", which lives in your own Claude Code
settings. No permission-bypass flag (`--dangerously-skip-permissions`,
`bypassPermissions`, `--dangerously-bypass-approvals-and-sandbox`,
`danger-full-access`) is ever passed; the CLI refuses to run a job whose
command line would hold one.

Nothing of a prompt, an answer or the CLI's own output is logged. A failure is
reported (locally and to the server) as one of `timeout`, `cli-missing`,
`not-logged-in`, `cli-error`, `bad-output`.

## Commands

```sh
livingbrain jobs agents            # which CLIs are installed and signed in
livingbrain jobs run               # run your jobs until Ctrl-C (one at a time)
livingbrain jobs run --once        # at most one job, then exit
livingbrain jobs run --agent codex # use Codex for every job
livingbrain --json jobs run        # one JSON event per line (the Mac app reads these)
```

Options: `--poll <s>` (default 5) between polls when idle, `--timeout <s>`
(default 300) per job, `--claude <path>` / `--codex <path>` when the binary is
not on `PATH`. A signed-out Living Brain token exits with status 2.

`--json` events: `started`, `claimed` (`job`, `kind`, `agent`, `model`),
`done` (`job`, `kind`, `seconds`, `prompt_tokens`, `completion_tokens`),
`failed` (`job`, `reason`), `error` (`message`; the runner keeps going with
backoff), `stopped`.

## Headless (a server, colony1, omarchy, any Linux box)

```sh
livingbrain login                 # device flow: approve in any browser
claude auth login                 # or: codex login — on this machine, once
livingbrain jobs run --agent claude
```

As a systemd user service:

```ini
# ~/.config/systemd/user/livingbrain-jobs.service
[Unit]
Description=Living Brain jobs on my Claude subscription
After=network-online.target

[Service]
ExecStart=%h/.local/bin/livingbrain jobs run --agent claude
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now livingbrain-jobs
journalctl --user -u livingbrain-jobs -f   # job ids, timings and token counts only
```

On a headless Linux box the CLI's token lives in the Secret Service keychain;
where there is none, set `LIVINGBRAIN_TOKEN` in the unit's environment from a
personal access token (Settings → Tokens).
