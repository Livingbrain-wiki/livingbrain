-- Personal access tokens can expire (issue #72, the OAuth connector sign-in),
-- Postgres form: identical to migrations/sqlite/0002_expiry.sql. NULL means
-- the token never expires, as every token minted before this column did.
ALTER TABLE personal_access_tokens ADD COLUMN expires_at TEXT;
