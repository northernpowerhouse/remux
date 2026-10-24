-- Movies and episodes with no TMDB/IMDb match, indexed under a local identity
-- (`opendal:{addon_id}:movie:…` / `opendal:{addon_id}:series:…`) when the
-- addon's `local_metadata` option is on, and when their last lookup ran.
ALTER TABLE opendal_files ADD COLUMN local_key TEXT;
ALTER TABLE opendal_files ADD COLUMN match_attempted_at TEXT;
CREATE INDEX IF NOT EXISTS idx_opendal_files_local_key
    ON opendal_files(addon_id, media_kind, local_key);
