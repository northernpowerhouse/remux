-- Metadata read from the sidecar files (NFO, `.plexmatch`) of a file indexed
-- under a local identity, as JSON. An episode row also carries its series'
-- and season's sidecar data.
ALTER TABLE opendal_files ADD COLUMN local_meta TEXT;
