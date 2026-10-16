-- The brain map's live feed (`GET /v1/graph/activity`): one row per read,
-- search or write of a page, so the map can show the brain being used.
--
-- Written best-effort by the MCP tools and the page reads, never by a path
-- that could fail the request it describes; pruned to a day of rows on each
-- write. `scope` and `slug` name the page (the node is `page:<scope>:<slug>`),
-- so a viewer only ever sees rows for pages their grants read. Nothing else
-- is kept: no body, no query text, no credential.
CREATE TABLE IF NOT EXISTS graph_activity (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT NOT NULL,
    scope        TEXT NOT NULL,
    slug         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    actor        TEXT NOT NULL,
    at           TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS graph_activity_by_workspace ON graph_activity (workspace_id, id);
CREATE INDEX IF NOT EXISTS graph_activity_by_time ON graph_activity (at);
