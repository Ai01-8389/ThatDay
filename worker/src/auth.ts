import { Hono } from "hono";
import { SignJWT, jwtVerify } from "jose";
import type { D1Database } from "@cloudflare/workers-types";

export interface AuthBindings {
  DB: D1Database;
  JWT_SECRET: string;
  MAILTRAP_API_KEY?: string;
  SUPER_ACCOUNTS?: string;
}

interface UserRow {
  id: string;
  email: string;
  tier: string;
}

export const auth = new Hono<{ Bindings: AuthBindings }>();

// ── Utilities ──

function generateCode(): string {
  const arr = new Uint8Array(3);
  crypto.getRandomValues(arr);
  return String(100000 + ((arr[0] << 16 | arr[1] << 8 | arr[2]) % 900000));
}

function validateEmail(email: string): boolean {
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email);
}

function getSecret(env: AuthBindings): Uint8Array {
  return new TextEncoder().encode(env.JWT_SECRET);
}

// ── JWT (30 天过期，路线 B) ──

export async function signJwt(
  env: AuthBindings,
  user: { id: string; email: string; tier: string }
): Promise<string> {
  return new SignJWT({ sub: user.id, email: user.email, tier: user.tier })
    .setProtectedHeader({ alg: "HS256" })
    .setExpirationTime("30d")
    .setIssuedAt()
    .sign(getSecret(env));
}

export async function verifyJwt(
  token: string,
  env: AuthBindings
): Promise<{ sub: string; email: string; tier: string } | null> {
  try {
    const { payload } = await jwtVerify(token, getSecret(env));
    return payload as { sub: string; email: string; tier: string };
  } catch {
    return null;
  }
}

// ── Auth middleware ──

export async function authMiddleware(c: any, next: any) {
  const header = c.req.header("Authorization");
  if (!header?.startsWith("Bearer ")) {
    return c.json({ error: "Unauthorized" }, 401);
  }
  const payload = await verifyJwt(header.slice(7), c.env);
  if (!payload) {
    return c.json({ error: "Invalid or expired token" }, 401);
  }
  c.set("userId", payload.sub);
  c.set("userEmail", payload.email);
  c.set("userTier", payload.tier);
  await next();
}

// ── Helper: find or create user ──

async function findOrCreateUser(
  db: D1Database,
  email: string,
  superAccounts: string
): Promise<UserRow> {
  const normalized = email.toLowerCase().trim();
  const existing = await db.prepare(
    "SELECT id, email, tier FROM users WHERE email = ?1"
  ).bind(normalized).first<UserRow>();

  if (existing) return existing;

  const id = crypto.randomUUID();
  const superList = superAccounts.split(",").map((s: string) => s.trim().toLowerCase());
  const tier = superList.includes(normalized) ? "super" : "free";

  // New user: start 7-day trial
  // Super accounts skip trial (no expiry)
  if (tier === "free") {
    await db.prepare(
      `INSERT INTO users (id, email, tier, trial_start, trial_end)
       VALUES (?1, ?2, ?3, datetime('now'), datetime('now', '+7 days'))`
    ).bind(id, normalized, tier).run();
  } else {
    await db.prepare(
      "INSERT INTO users (id, email, tier) VALUES (?1, ?2, ?3)"
    ).bind(id, normalized, tier).run();
  }

  return { id, email: normalized, tier };
}

// ── POST /auth/request-otp ──

auth.post("/request-otp", async (c) => {
  const { email } = await c.req.json<{ email?: string }>();
  if (!email || !validateEmail(email)) {
    return c.json({ error: "Valid email is required" }, 400);
  }

  const normalized = email.toLowerCase().trim();
  const superAccounts = c.env.SUPER_ACCOUNTS || "";

  // Find or create user (new users auto-registered on first OTP request)
  const user = await findOrCreateUser(c.env.DB, normalized, superAccounts);

  // Rate limit: max 3 OTP requests per 60 seconds per user
  const rateResult = await c.env.DB.prepare(
    `SELECT COUNT(*) as cnt FROM auth_sessions
     WHERE user_id = ?1 AND session_type = 'otp'
       AND created_at > datetime('now', '-60 seconds')`
  ).bind(user.id).first<{ cnt: number }>();

  if (rateResult && rateResult.cnt >= 3) {
    return c.json({ error: "Too many requests. Please wait and try again." }, 429);
  }

  // Generate OTP code, store in auth_sessions (5 min TTL)
  const code = generateCode();
  const sessionId = crypto.randomUUID();
  const expiresAt = new Date(Date.now() + 5 * 60 * 1000).toISOString();

  await c.env.DB.prepare(
    `INSERT INTO auth_sessions (id, user_id, token, session_type, expires_at, attempts, verified)
     VALUES (?1, ?2, ?3, 'otp', ?4, 0, 0)`
  ).bind(sessionId, user.id, code, expiresAt).run();

  // Send OTP via Mailtrap
  if (c.env.MAILTRAP_API_KEY) {
    const res = await fetch("https://send.api.mailtrap.io/api/send", {
      method: "POST",
      headers: {
        Authorization: "Bearer " + c.env.MAILTRAP_API_KEY,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        from: { email: "hello@thatday.vip", name: "That Day" },
        to: [{ email: normalized }],
        subject: "Your That Day verification code",
        html: [
          "<p>Your verification code is:</p>",
          `<p style="font-size:28px;font-weight:bold;letter-spacing:4px">${code}</p>`,
          "<p style=\"color:#888\">This code expires in 5 minutes.</p>",
        ].join(""),
        category: "otp",
      }),
    });
    if (!res.ok) {
      return c.json({ error: "Failed to send verification email" }, 500);
    }
    return c.json({ success: true, message: "Verification code sent to your email" });
  }

  // DEV fallback (no Mailtrap key configured)
  return c.json({ success: true, message: "[DEV] Verification code: " + code });
});

// ── POST /auth/verify-otp ──

auth.post("/verify-otp", async (c) => {
  const { email, code, utc_offset } = await c.req.json<{ email?: string; code?: string; utc_offset?: number }>();
  if (!email || !code) {
    return c.json({ error: "Email and code are required" }, 400);
  }

  const normalized = email.toLowerCase().trim();

  // Find user
  const user = await c.env.DB.prepare(
    "SELECT id, email, tier, trial_start, trial_end, utc_offset_minutes FROM users WHERE email = ?1"
  ).bind(normalized).first<{ id: string; email: string; tier: string; trial_start?: string; trial_end?: string; utc_offset_minutes?: number }>();

  if (!user) {
    return c.json({ error: "No verification code found. Please request a new one." }, 400);
  }

  // Save timezone offset if provided (desktop detects on login)
  if (typeof utc_offset === "number") {
    await c.env.DB.prepare(
      "UPDATE users SET utc_offset_minutes = ?1 WHERE id = ?2"
    ).bind(utc_offset, user.id).run();
    user.utc_offset_minutes = utc_offset;
  }

  // Compute trial status
  const trial_active = user.trial_end
    ? new Date(user.trial_end + "Z") > new Date()
    : (user.tier === "free" ? false : undefined);

  // Find latest unverified OTP session for this user
  const otpSession = await c.env.DB.prepare(
    `SELECT id, token, attempts, expires_at FROM auth_sessions
     WHERE user_id = ?1 AND session_type = 'otp' AND verified = 0
       AND expires_at > datetime('now')
     ORDER BY created_at DESC LIMIT 1`
  ).bind(user.id).first<{ id: string; token: string; attempts: number; expires_at: string }>();

  if (!otpSession) {
    return c.json({ error: "Verification code expired or not found. Please request a new one." }, 400);
  }

  // Check attempts (max 5)
  if (otpSession.attempts >= 5) {
    await c.env.DB.prepare(
      "DELETE FROM auth_sessions WHERE id = ?1"
    ).bind(otpSession.id).run();
    return c.json({ error: "Too many failed attempts. Please request a new code." }, 400);
  }

  // Verify code
  if (otpSession.token !== code) {
    await c.env.DB.prepare(
      "UPDATE auth_sessions SET attempts = attempts + 1 WHERE id = ?1"
    ).bind(otpSession.id).run();
    return c.json({ error: "Invalid verification code" }, 400);
  }

  // Mark OTP session as verified
  await c.env.DB.prepare(
    "UPDATE auth_sessions SET verified = 1 WHERE id = ?1"
  ).bind(otpSession.id).run();

  // Issue JWT (30 days)
  const token = await signJwt(c.env, user);

  // Store login session
  const loginSessionId = crypto.randomUUID();
  const loginExpiresAt = new Date(Date.now() + 30 * 24 * 3600 * 1000).toISOString();
  await c.env.DB.prepare(
    `INSERT INTO auth_sessions (id, user_id, token, session_type, expires_at, verified)
     VALUES (?1, ?2, ?3, 'login', ?4, 1)`
  ).bind(loginSessionId, user.id, token, loginExpiresAt).run();

  return c.json({
    token,
    user: { id: user.id, email: user.email, tier: user.tier, trial_start: user.trial_start, trial_end: user.trial_end, trial_active, utc_offset_minutes: user.utc_offset_minutes },
  });
});

// ── GET /auth/me ──

auth.get("/me", async (c) => {
  const header = c.req.header("Authorization");
  if (!header?.startsWith("Bearer ")) {
    return c.json({ error: "Unauthorized" }, 401);
  }
  const payload = await verifyJwt(header.slice(7), c.env);
  if (!payload) {
    return c.json({ error: "Invalid or expired token" }, 401);
  }
  const user = await c.env.DB.prepare(
    "SELECT id, email, tier, created_at, trial_start, trial_end, utc_offset_minutes FROM users WHERE id = ?1"
  ).bind(payload.sub).first();
  if (!user) {
    return c.json({ error: "User not found" }, 404);
  }
  // Compute trial status
  const trial_active = user.trial_end
    ? new Date(user.trial_end + "Z") > new Date()
    : false;
  return c.json({ user: { ...user, trial_active } });
});
