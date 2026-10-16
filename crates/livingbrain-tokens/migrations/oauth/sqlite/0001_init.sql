-- The OAuth 2.1 authorization server behind the MCP connector sign-in (issue
-- #72): dynamically registered public clients, one-time authorization codes,
-- and rotating refresh tokens. The access token a grant mints is an ordinary
-- personal access token (`personal_access_tokens`), so every route that
-- accepts a PAT accepts it without knowing OAuth exists.
--
-- Nothing here holds a credential in the clear: a code and a refresh token
-- are stored as the lowercase-hex SHA-256 of the whole value, the same
-- rule the PAT table follows.

-- A public client (auth method `none`): a registration names no secret,
-- because a client that cannot keep one could not keep it from the browser
-- either. `redirect_uris` is a JSON array of the exact URIs it may return to.
CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id     TEXT PRIMARY KEY,
    client_name   TEXT NOT NULL DEFAULT '',
    redirect_uris TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    -- SHA-256 of the registering address (never the address), for the per-hour
    -- registration limit; 1 when a redirect host is not one Living Brain
    -- recognises as Claude, for the daily cap that counts only those.
    ip_hash       TEXT NOT NULL DEFAULT '',
    unverified    INTEGER NOT NULL DEFAULT 0
);

-- One authorization code: single use (`used_at` is set by the consuming
-- UPDATE, which only one request can win), sixty seconds to live, bound to
-- the client, the exact redirect URI, the PKCE challenge, and the member it
-- was approved for. `family` names the connection the code opens: every
-- token that connection ever holds shares it, so a replayed code revokes the
-- whole chain, not just the first token it made.
CREATE TABLE IF NOT EXISTS oauth_codes (
    code_hash      TEXT PRIMARY KEY,
    client_id      TEXT NOT NULL,
    redirect_uri   TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    workspace_id   TEXT NOT NULL,
    user_id        TEXT NOT NULL,
    scope          TEXT NOT NULL DEFAULT '',
    -- The connector label the token will carry, decided by the redirect URI
    -- this code is being issued for, not by the client's other URIs.
    name           TEXT NOT NULL DEFAULT '',
    family         TEXT NOT NULL,
    expires_at     TEXT NOT NULL,
    used_at        TEXT,
    created_at     TEXT NOT NULL
);

-- One refresh token. `prefix` is the access token it was issued beside, and
-- `family` ties every refresh token of one connection together. A refresh is
-- refused once its access token is revoked, so revoking a connection in the
-- settings list ends its renewal too. Rotation sets `used_at` on the spent
-- token and mints the next in the same family; a spent token presented again
-- is a replay, and revokes every access token of the family.
CREATE TABLE IF NOT EXISTS oauth_refresh_tokens (
    token_hash   TEXT PRIMARY KEY,
    family       TEXT NOT NULL,
    prefix       TEXT NOT NULL,
    client_id    TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    used_at      TEXT,
    name         TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS oauth_refresh_tokens_family
    ON oauth_refresh_tokens (family);
