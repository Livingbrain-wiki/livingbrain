-- Dreaming covers a whole subject, in windows (docs/temporal-facts.md,
-- "Dreaming").
--
-- `cursor` is where the next window of a subject starts: `created_at|id` of
-- the last current fact a pass reviewed. A subject with more facts than one
-- window is re-queued at the end of each pass with its cursor, so the next
-- tick reviews the next window; the cursor is cleared when a sweep reaches
-- the end. NULL is "from the start", which is also every row that predates
-- this column.
ALTER TABLE fact_dreams ADD COLUMN cursor TEXT;
