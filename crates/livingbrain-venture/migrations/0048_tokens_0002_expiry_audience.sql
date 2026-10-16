-- Personal access tokens: expiry and audience (issue #72, the OAuth connector
-- sign-in).
--
-- `expires_at` is an ISO-8601 stamp after which the token is refused. NULL
-- means it never expires, which is every token minted before this column.
--
-- `audience` says which surfaces a token is accepted by. NULL is an ordinary
-- token (the CLI, the settings page, a coding agent): every bearer surface
-- accepts it. 'mcp' is a connector token minted by the OAuth sign-in. It is
-- accepted by the MCP endpoint and by nothing else, so a connected app cannot
-- use it to reach the settings, admin or write routes.
ALTER TABLE personal_access_tokens ADD COLUMN expires_at TEXT;
ALTER TABLE personal_access_tokens ADD COLUMN audience TEXT;
