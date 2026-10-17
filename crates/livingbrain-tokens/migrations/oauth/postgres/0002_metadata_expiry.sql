-- Client ID Metadata Document clients (issue #234), Postgres form: identical
-- to migrations/sqlite/0002_metadata_expiry.sql. `metadata_expires_at` is
-- when a cached client metadata document must be fetched again; NULL is a
-- dynamically registered client, which never expires.
ALTER TABLE oauth_clients ADD COLUMN metadata_expires_at TEXT;
