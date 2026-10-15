-- Any provider in the catalog (issue: every provider Colonizer supports).
-- A connection now records how to talk to its provider: `auth` is how the
-- key travels (`bearer` = Authorization: Bearer, `x-api-key` = Anthropic's
-- header) and `wire` is the API it speaks (`openai` chat completions or
-- `anthropic` Messages). Every row written before this was an OpenAI-wire,
-- bearer connection, which is what the defaults say.
ALTER TABLE model_connections ADD COLUMN auth TEXT NOT NULL DEFAULT 'bearer';
ALTER TABLE model_connections ADD COLUMN wire TEXT NOT NULL DEFAULT 'openai';
