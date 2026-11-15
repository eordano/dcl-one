-- The client address bucket of a presign (an IPv4 address or an IPv6 /64),
-- for one address's share of the node's bound on report evidence. Existing
-- slots keep NULL: they count for the node and for no address. No backfill;
-- the object pass creates the two partial indexes.
ALTER TABLE IF EXISTS social.player_report_evidence
    ADD COLUMN IF NOT EXISTS source_key text;
