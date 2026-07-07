-- ThatDay Route B v1.2 — 4 张表
-- Phase 1 底座

-- 用户表
CREATE TABLE IF NOT EXISTS users (
  id         TEXT PRIMARY KEY,
  email      TEXT NOT NULL UNIQUE,
  tier       TEXT DEFAULT 'free',    -- 'free' | 'pro' | 'super'
  created_at TEXT DEFAULT (datetime('now'))
);

-- 认证会话（OTP + JWT 共用，session_type 区分）
CREATE TABLE IF NOT EXISTS auth_sessions (
  id           TEXT PRIMARY KEY,
  user_id      TEXT NOT NULL REFERENCES users(id),
  token        TEXT NOT NULL,
  session_type TEXT DEFAULT 'login', -- 'otp' | 'login'
  expires_at   TEXT NOT NULL,
  attempts     INTEGER DEFAULT 0,    -- OTP 重试次数
  verified     INTEGER DEFAULT 0,    -- OTP 是否已验证
  created_at   TEXT DEFAULT (datetime('now'))
);

-- 当日标注（临时中转 + 离线同步队列）
CREATE TABLE IF NOT EXISTS daily_annotations (
  id            TEXT PRIMARY KEY,
  user_id       TEXT NOT NULL,
  calendar_date TEXT NOT NULL,       -- '2026-06-22'
  photos_json   TEXT NOT NULL,       -- [{time, gps, who, where, event, scene_tags, season, time_of_day, ...}]
  status        TEXT DEFAULT 'pending',  -- pending | processing | sent | sending_failed
  created_at    TEXT DEFAULT (datetime('now')),
  updated_at    TEXT DEFAULT (datetime('now')),
  UNIQUE(user_id, calendar_date)     -- 每人每天一行
);

CREATE INDEX IF NOT EXISTS idx_annotations_status ON daily_annotations(status);
CREATE INDEX IF NOT EXISTS idx_annotations_user_date ON daily_annotations(user_id, calendar_date);

-- 故事存档（永久保留）
CREATE TABLE IF NOT EXISTS stories (
  id            TEXT PRIMARY KEY,
  user_id       TEXT NOT NULL,
  calendar_date TEXT NOT NULL,
  title         TEXT,
  content       TEXT NOT NULL,        -- 完整故事文本
  photos_json   TEXT,                 -- 标注快照（用于明年今天复现）
  created_at    TEXT DEFAULT (datetime('now')),
  UNIQUE(user_id, calendar_date)
);
