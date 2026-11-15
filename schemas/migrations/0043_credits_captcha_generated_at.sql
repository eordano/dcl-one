-- Record the last challenge presentation time for the minimum solve duration.
-- Existing challenges start their duration at migration; balances are untouched.
ALTER TABLE IF EXISTS credits.captcha_challenges
    ADD COLUMN IF NOT EXISTS generated_at timestamp without time zone
        NOT NULL DEFAULT timezone('UTC', now());
