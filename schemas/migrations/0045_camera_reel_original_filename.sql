-- A photo retains its multipart basename so gallery URLs keep the upload's
-- UUID-prefixed image and thumbnail names. Alias reads extract the UUID and use
-- the existing primary key; blob bytes remain hash-addressed in shared storage.
-- Existing rows keep NULL and their current hash URLs. No backfill or index.
ALTER TABLE IF EXISTS camera_reel.images
    ADD COLUMN IF NOT EXISTS original_filename text;
