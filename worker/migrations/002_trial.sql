-- ThatDay v0.3 — 7-day trial tracking
-- Self-managed trial (not Creem); users get 7 days free, no credit card

ALTER TABLE users ADD COLUMN trial_start TEXT;
ALTER TABLE users ADD COLUMN trial_end TEXT;

-- Existing users (before migration): no trial tracking, treated as legacy (trial_active = false)
