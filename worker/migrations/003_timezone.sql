-- ThatDay v0.3 — user timezone support
-- Stores the user's UTC offset in minutes.
-- Default 480 (UTC+8 = Beijing) as initial user base is primarily Asia.

ALTER TABLE users ADD COLUMN utc_offset_minutes INTEGER DEFAULT 480;
