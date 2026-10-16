# Discord connection

A workspace admin connects a Discord server, and Living Brain imports its
messages: the whole history first, then new messages as they arrive. Each
channel's messages are filed one UTC day at a time, as sources the compiler
reads. The crate is `crates/livingbrain-discord`; its routes are under
`/v1/discord`; the settings card is in Settings → Integrations.

## What is imported

- **The text channels and threads you include** in the server. New servers
  start with every readable text channel and thread included, except channels
  marked as adult (NSFW). Turn any of them off, or on, in Settings → Discord.
- **Messages from everyone in those channels**, not only the admin who
  connected the server. Message text, the author's display name, the time, the
  reply a message answers, and attachment names and links.
- **Messages from bots** are imported as text, marked `(bot)`. System messages
  (joins, pins, boosts, thread notices) are not.
- Nothing is read from a channel that is excluded, from a server that is
  disconnected, or from a channel the bot cannot see (Discord refuses it and the
  channel shows as unavailable).
- Secrets and email addresses are redacted by the same pipeline every import
  uses, before anything is stored.

Disconnecting stops reading at once. The messages already imported stay. The
bot stays in the server until someone removes it on Discord (Server Settings →
Integrations): a bot cannot leave a server by itself.

## Operator setup

1. **Create the app.** At https://discord.com/developers/applications, create an
   application named for Living Brain. Note its **Application ID**.
2. **Add the bot.** Under *Bot*, add a bot user and reset its token. Copy the
   token once; it is a secret.
3. **Enable the message content intent.** Still under *Bot*, turn on
   **MESSAGE CONTENT INTENT** (a privileged intent). Without it Discord sends
   message events with no text, and the import would store empty messages. It is
   free for an app in fewer than 100 servers; beyond that Discord requires app
   verification first.
4. **Set the redirect.** Under *OAuth2 → General*, add the redirect URL
   `https://app.livingbrain.wiki/v1/discord/callback` (and the staging origin's
   `/v1/discord/callback`). It must match the origin in `WORKSPACES_REDIRECT_BASE`.
5. **Permissions.** The install link asks for exactly one permission integer,
   **66560**: `VIEW_CHANNEL` (1024) and `READ_MESSAGE_HISTORY` (65536). Nothing
   else. The bot posts, reacts, and manages nothing.
6. **Set the three credentials** (names only, never values in the repo):

   | name | kind | where |
   |---|---|---|
   | `DISCORD_CLIENT_ID` | public | `[vars]` in `crates/livingbrain-venture/wrangler.toml`, replacing the placeholder, per environment |
   | `DISCORD_CLIENT_SECRET` | secret | `wrangler secret put DISCORD_CLIENT_SECRET --env <env>` |
   | `DISCORD_BOT_TOKEN` | secret | `wrangler secret put DISCORD_BOT_TOKEN --env <env>` |

   Until all three are real values the integration is unavailable: the settings
   section stays hidden and `/v1/discord/connect` answers 503. A placeholder
   (`replace-with-…`, `changeme`, `xxx`) counts as not set.
7. **Migrations.** The table migration is
   `crates/livingbrain-venture/migrations/0045_discord_0001_init.sql`, applied
   with the other migrations (`wrangler d1 migrations apply`).
8. **Optional tuning.** `DISCORD_TICK_BUDGET` (default 200) is the most Discord
   calls one cron tick may make.

The bot is the only identity the import uses. There is no user token and no
self-bot, and nothing is scraped.

## How it runs

Backfill and import run **only inside the Worker's cron** (every five minutes).
No route, CLI command or script starts one. A server is connected by an admin
through the card; the first tick then lists its channels (a few minutes).

Each tick, within its call budget:

1. **Channel list.** A server's channels and active threads are re-read at most
   hourly; each text channel's archived threads are listed a page at a time.
2. **Backfill.** Each included channel is read backwards (`before=`, 100
   messages a page, at most five pages a tick) until Discord returns an empty
   page. The channel's oldest and newest message ids are kept as cursors.
3. **Forward poll.** Once a channel's history is done, new messages are read
   forwards (`after=` the newest id, at most three pages a tick).
4. **Filing.** Every message marks its UTC day dirty. A dirty day is re-read in
   full and filed as one transcript (at most 20 days a tick). A busy day is
   refiled at most once an hour; a closed day (before today) is filed at once.

A channel is leased while a tick reads it, so overlapping ticks never read the
same channel twice.

**Rate limits.** Every answer's `X-RateLimit-Remaining`, `X-RateLimit-Reset-After`
and `Retry-After` are honoured. A `429`, an exhausted bucket, a `5xx` or a
network failure stops all calls for the rest of that tick; the next tick resumes
from the stored cursors. Nothing sleeps. A `403` or `404` on one channel is
recorded on that channel (with a retry time for a `403`) and the tick goes on.

## What a source looks like

One source per channel per UTC day, filed in the workspace's shared scope:

```
# #general in Test Guild

UTC day 2026-10-03 · 3 messages

**09:14:02 · Alice**
Shipping the fix today.

**09:20:55 · Bob (bot)** · reply to Alice
[attachment: build.log · https://cdn.discordapp.com/…]

**10:01:37 · Carol**
Thanks, merged.
```

- `rel_path`: `discord/<server id>/<channel id>/<YYYY-MM-DD>`.
- `origin_ref`: the channel's permalink.
- `project`: the server's name, slugged, when it is a valid project name.
- `imported_by`: the member who connected the server.

## Limits and open questions

- **Versions accumulate.** When a day changes, its new transcript is a new
  source row; the earlier version stays in the ledger, because the ledger has no
  way to supersede a source yet. Each new version is also queued for compilation.
  The hourly throttle keeps a busy day to about 24 versions a day at most. A
  supersede operation in `livingbrain-pages` would end this; until then, prune
  old versions by hand if they matter.
- **Permissions are learned, not checked.** Discord lists channels whether or not
  the bot may read them. A channel the bot cannot read is recorded as
  unavailable after its first refused read, and retried hourly.
- **Forum and media channels** are imported through their posts (threads), not
  as channels; their archived posts are not listed.
- **The connect callback proves the bot is in the server, not who clicked
  Discord's button.** Only a state minted for the admin, spent once, is accepted,
  and Discord must confirm the bot is a member of the named server. A stronger
  check would need the user's Discord identity (`identify`), which the bot-only
  flow does not ask for.
- **Budgets.** The import does not yet count against a workspace's token or
  budget caps; GitHub's import does not either.
