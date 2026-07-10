import { Hono } from "hono";
import { cors } from "hono/cors";
import type { Env, Variables } from "./types";
import { auth, authMiddleware } from "./auth";
import { annotations } from "./annotations";
import { story, generateStory } from "./story";
import { payment } from "./payment";
import { sendStoryEmail } from "./email";
import { media } from "./media";

// ── Types ──
// Env / Variables are defined in ./types and shared across all routes.

const app = new Hono<{ Bindings: Env; Variables: Variables }>();

// ── Global middleware ──

// CORS: allow tauri desktop + web (MVP: allow all origins)
app.use("*", cors({
  origin: (origin) => {
    // Accept all origins in MVP mode; Tauri webview may send various origins
    const allowed = ["https://thatday.vip", "https://www.thatday.vip", "tauri://localhost", "http://localhost:5173", "https://tauri.localhost"];
    if (!origin || allowed.includes(origin)) return origin;
    // Be permissive: return the actual origin to avoid CORS blocks in built Tauri app
    return origin;
  },
  allowMethods: ["GET", "POST", "PUT", "DELETE", "OPTIONS"],
  allowHeaders: ["Content-Type", "Authorization"],
  maxAge: 86400,
}));

// ── Public routes ──

app.get("/health", (c) => c.json({ status: "ok", time: new Date().toISOString() }));

// Desktop download: redirect to latest .msi in R2
app.get("/download", async (c) => {
  const key = "releases/thatday.msi";
  const head = await c.env.R2.head(key);
  if (!head) return c.text("No installer available yet. Check back soon.", 404);
  try {
    const url = await c.env.R2.createSignedUrl({ key, method: "GET", signedHeaders: new Headers(), expiresIn: 3600 });
    return c.redirect(url, 302);
  } catch {
    const obj = await c.env.R2.get(key);
    if (!obj) return c.text("Not found", 404);
    c.header("Content-Type", "application/octet-stream");
    c.header("Content-Disposition", "attachment; filename=thatday.msi");
    return c.body(obj.body);
  }
});

app.route("/auth", auth);

// ── Media upload/download (PDF + Audio via R2) ──
app.route("/media", media);
app.route("/pdf", media);
app.route("/audio", media);

// ── Protected routes ──

app.use("/annotations/*", authMiddleware);
app.route("/annotations", annotations);

app.use("/generate-story", authMiddleware);
app.use("/send-story", authMiddleware);
app.use("/stories", authMiddleware);
app.route("/", story); // story routes: /generate-story, /send-story, /stories, /stories/:date

// Payment webhook (Creem signature auth, no JWT middleware)
app.route("/payment", payment);

// ── Cron Triggers ──
//
// Two morning crons to cover global timezones:
//   0:00 UTC → covers Asia (UTC+6 ~ UTC+12) at their ~8 AM
//  12:00 UTC → covers Americas + Europe (UTC-6 ~ UTC+2) at their ~8 AM
//   19:00 UTC → cleanup

export default {
  fetch: app.fetch,

  async scheduled(event: ScheduledEvent, env: Env) {
    const cron = event.cron;

    // Story generation — morning for Asia
    if (cron === "0 0 * * *") {
      await morningCron(env, 0);
    }

    // Story generation — morning for Americas + Europe
    if (cron === "0 12 * * *") {
      await morningCron(env, 12);
    }

    // Cleanup
    if (cron === "0 19 * * *") {
      await cleanupCron(env);
    }
  },
};

// ── Morning Cron: send emails for stories ready to deliver ──
// Stories are generated at Save/Seal time, Cron only sends emails.

async function morningCron(env: Env, cronUtcHour: number) {
  if (!env.MAILTRAP_API_KEY) {
    console.error("Cron skipped: MAILTRAP_API_KEY not configured");
    return;
  }

  // Fetch generated (not yet emailed) + pending (old saves, generate now as fallback)
  const { results } = await env.DB.prepare(
    `SELECT da.*, u.utc_offset_minutes, u.email
     FROM daily_annotations da
     JOIN users u ON da.user_id = u.id
     WHERE da.status IN ('generated', 'pending', 'sending_failed')
     ORDER BY da.user_id, da.calendar_date`
  ).all();

  if (!results || results.length === 0) {
    console.log(`No generated/pending annotations for UTC ${cronUtcHour}:00 cron`);
    return;
  }

  // Filter: only process users whose local time is 7-9 AM AND whose
  // calendar_date matches today in the user's local timezone
  const toProcess: any[] = [];
  for (const row of results as any[]) {
    const offset = row.utc_offset_minutes ?? 480;
    const localHour = (cronUtcHour + Math.floor(offset / 60) + 24) % 24;
    if (localHour >= 7 && localHour <= 9) {
      const utcNow = new Date();
      const userNow = new Date(utcNow.getTime() + offset * 60 * 1000);
      const userToday = userNow.toISOString().slice(0, 10); // YYYY-MM-DD
      if (row.calendar_date !== userToday) continue;
      toProcess.push(row);
    }
  }

  if (toProcess.length === 0) {
    console.log(`No users in 7-9 AM window for UTC ${cronUtcHour}:00 cron`);
    return;
  }

  console.log(`Cron: ${toProcess.length} annotations to process for UTC ${cronUtcHour}:00`);

  for (const row of toProcess) {
    const userId = row.user_id;

    try {
      let storyRow: any;

      if (row.status === 'generated') {
        // Story already generated — just send email
        storyRow = await env.DB.prepare(
          "SELECT id, title, content, calendar_date FROM stories WHERE user_id = ?1 AND calendar_date = ?2"
        ).bind(userId, row.calendar_date).first();
      }

      if (!storyRow && (row.status === 'pending' || row.status === 'sending_failed')) {
        // Fallback: old Save without generation or previous failure — generate now
        if (!env.DEEPSEEK_API_KEY) continue;
        await env.DB.prepare(
          "UPDATE daily_annotations SET status = 'processing', updated_at = datetime('now') WHERE id = ?1"
        ).bind(row.id).run();

        const storyResult = await generateStory(env, row);
        const storyId = crypto.randomUUID();
        await env.DB.prepare(
          `INSERT INTO stories (id, user_id, calendar_date, title, content, photos_json)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6)
           ON CONFLICT(user_id, calendar_date) DO UPDATE SET
             title = excluded.title, content = excluded.content, photos_json = excluded.photos_json`
        ).bind(storyId, userId, row.calendar_date, storyResult.title, storyResult.content, row.photos_json).run();
        storyRow = { id: storyId, title: storyResult.title, content: storyResult.content, calendar_date: row.calendar_date };
      }

      if (!storyRow) continue;

      // Send email
      const emailSent = await sendStoryEmail(env, userId, storyRow);

      if (emailSent) {
        await env.DB.prepare(
          "UPDATE daily_annotations SET status = 'sent', updated_at = datetime('now') WHERE id = ?1"
        ).bind(row.id).run();
        console.log(`Email sent for user ${userId}, date ${row.calendar_date}`);
      } else {
        console.error(`Email failed for user ${userId}, date ${row.calendar_date}`);
      }
    } catch (err: any) {
      console.error(`Cron failed for user ${userId}, date ${row.calendar_date}:`, err.message);
    }
  }
}

// ── Cleanup Cron: delete sent annotations older than 2 days ──

async function cleanupCron(env: Env) {
  const result = await env.DB.prepare(
    `DELETE FROM daily_annotations
     WHERE status = 'sent'
       AND created_at < datetime('now', '-2 days')`
  ).run();

  console.log(`Cleaned up ${result.meta?.changes || 0} sent annotations`);
}
