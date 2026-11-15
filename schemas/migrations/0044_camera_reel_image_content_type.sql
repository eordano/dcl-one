-- Preserve the accepted upload MIME for both UUID filename aliases. A download
-- reads it with the existing photo primary-key lookup; no index or backfill.
-- Old rows remain NULL because their original object metadata is not known.
-- The CHECK is a table constraint added NOT VALID (a column constraint cannot
-- be), so adding it takes no lock-holding scan of existing rows. Those rows
-- hold NULL, which the CHECK admits; every later write is checked.
ALTER TABLE IF EXISTS camera_reel.images ADD COLUMN IF NOT EXISTS image_content_type text;
ALTER TABLE IF EXISTS camera_reel.images ADD CONSTRAINT images_image_content_type_check CHECK (image_content_type IN ('image/png', 'image/jpeg')) NOT VALID;
