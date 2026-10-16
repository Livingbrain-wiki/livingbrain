-- Personal access tokens can expire (issue #72, the OAuth connector sign-in).
-- A token minted by the authorization-code grant lives an hour and is
-- renewed through its refresh token; `expires_at` is an ISO-8601 stamp, NULL
-- for every token that never expires (the CLI's device tokens and every
-- token minted before this column existed). A NULL keeps the old meaning, so
-- nothing already minted changes.
ALTER TABLE personal_access_tokens ADD COLUMN expires_at TEXT;
