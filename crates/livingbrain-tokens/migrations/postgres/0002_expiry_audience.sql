-- Personal access tokens: expiry and audience (issue #72), Postgres form:
-- identical to migrations/sqlite/0002_expiry_audience.sql. NULL expiry means
-- the token never expires; NULL audience is an ordinary token, 'mcp' a
-- connector token accepted by the MCP endpoint only.
ALTER TABLE personal_access_tokens ADD COLUMN expires_at TEXT;
ALTER TABLE personal_access_tokens ADD COLUMN audience TEXT;
