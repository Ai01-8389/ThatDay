import { Hono } from "hono";
import type { Env, Variables } from "./types";
import { authMiddleware } from "./auth";

interface AnnotationRow {
  id: string;
  user_id: string;
  calendar_date: string;
  photos_json: string;
  status: string;
}

export const story = new Hono<{ Bindings: Env; Variables: Variables }>();

// Apply auth middleware directly within this sub-router for all /stories routes
story.use("/stories/*", authMiddleware);
story.use("/generate-story", authMiddleware);
story.use("/send-story", authMiddleware);

// ── Shared: AI story generation (used by /generate-story and Cron) ──

export async function generateStory(
  env: Env,
  row: AnnotationRow
): Promise<{ title: string; content: string }> {
  const apiKey = env.DEEPSEEK_API_KEY;
  if (!apiKey) {
    throw new Error("DeepSeek API key not configured");
  }

  let photos: any[];
  try {
    photos = JSON.parse(row.photos_json);
  } catch {
    throw new Error("Invalid photos_json");
  }

  if (!photos || photos.length === 0) {
    throw new Error("No photos in annotation");
  }

  // ── Extract date facts ──
  const [yearStr, monthStr, dayStr] = row.calendar_date.split("-");
  const month = parseInt(monthStr);
  const season = month ? getSeason(month) : "";

  // ── Best-effort weather (2-level fallback) ──
  let weatherCoords: { lat: number; lon: number } | null = null;

  const photoWithGps = photos.find((p: any) => p.gps?.lat && p.gps?.lon);
  if (photoWithGps?.gps) {
    weatherCoords = { lat: photoWithGps.gps.lat, lon: photoWithGps.gps.lon };
  }
  if (!weatherCoords) {
    const wherePlace = photos.find((p: any) => p.where)?.where;
    if (wherePlace) {
      try { weatherCoords = await geocodePlace(wherePlace); } catch { /* skip */ }
    }
  }

  // ── Fetch per-year weather (each year gets its own historical weather) ──
  const weatherByYear: Record<string, string> = {};
  if (weatherCoords) {
    const years = [...new Set(photos.map((p: any) => p.time?.slice(0, 4)).filter(Boolean))] as string[];
    for (const y of years) {
      const dateForYear = `${y}-${monthStr}-${dayStr}`;
      try {
        const w = await fetchWeather(weatherCoords.lat, weatherCoords.lon, dateForYear);
        if (w) weatherByYear[y] = w;
      } catch { /* skip this year */ }
    }
  }

  // ── Context: group by year, use time-of-day labels, no date repetition ──
  const byYear: Record<string, string[]> = {};
  for (const p of photos) {
    if (!p.time) continue;
    const y = p.time.slice(0, 4);
    if (!byYear[y]) byYear[y] = [];

    const timeLabel = timeOfDayLabel(p.time);
    const parts: string[] = [timeLabel];
    if (p.who) parts.push(`with ${p.who}`);
    if (p.where) parts.push(`at ${p.where}`);
    if (p.event) parts.push(p.event);
    if (p.scene_tags) {
      const tags = Object.entries(p.scene_tags)
        .filter(([, v]) => v)
        .map(([k]) => k.replace("is_", "").replace("has_", ""));
      if (tags.length > 0) parts.push(`[${tags.join(", ")}]`);
    }
    byYear[y].push(`  - ${parts.join(", ")}`);
  }

  const sortedYears = Object.keys(byYear).sort();
  const contextLines = sortedYears.map(y => {
    const w = weatherByYear[y];
    return `${y}:${w ? ` (${w})` : ""}\n${byYear[y].join("\n")}`;
  }).join("\n\n");

  const systemPrompt = `You help people recall their memories. That Day shows photos from 
"on this day" across many years. The user annotated each photo with who, where, 
and what happened.

Write in English only. Keep any non-English names as-is.

IMPORTANT: Do NOT write one unified story. Write one short section per year. 
Each section is an independent vignette. No connections, no transitions, 
no overarching narrative. No introduction, no conclusion.

Format each section like this:
YYYY.M.D weekday time_of_day, weather, who was there, what they did... → the photo moment

Rules:
- One section per year. Separate sections with a blank line.
- Sort from earliest year to latest year.
- Each section starts with: YYYY.M.D weekday time_of_day, weather
- Then describe who was there, doing what, with 1–2 sensory details (light, heat, sound)
- End each section by describing the camera moment itself
- Use the EXACT names, places, activities the user wrote. Don't invent.
- Keep each section 40–70 words. The whole output concise — like flipping through an album.
- Warm tone, present tense. The feeling of looking at old photos.
- Do NOT write a beginning or ending paragraph.
- Do NOT repeat the date within a section`;

  const userPrompt = `Write warm memory vignettes for these photos, one section per year. Use only the provided annotations:

${row.calendar_date}
${contextLines}`;

  const response = await fetch("https://api.deepseek.com/v1/chat/completions", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${apiKey}`,
    },
    body: JSON.stringify({
      model: "deepseek-chat",
      messages: [
        { role: "system", content: systemPrompt },
        { role: "user", content: userPrompt },
      ],
      max_tokens: 2000,
      temperature: 0.8,
    }),
  });

  if (!response.ok) {
    const errText = await response.text();
    console.error("DeepSeek story error:", errText);
    throw new Error(`DeepSeek ${response.status}: ${errText.slice(0, 200)}`);
  }

  const data: any = await response.json();
  let raw = (data.choices?.[0]?.message?.content || "").trim();
  if (!raw) throw new Error("Empty response from DeepSeek");

  // ── JSON extraction (robust) ──
  // Strip markdown fences
  raw = raw.replace(/^```(?:json)?\s*\n?/i, "").replace(/\n?```\s*$/, "");
  raw = raw.trim();

  // Try direct parse
  try {
    const parsed = JSON.parse(raw);
    if (parsed.title && parsed.content) {
      return { title: sanitizeTitle(parsed.title), content: sanitizeContent(parsed.content) };
    }
  } catch { /* fall through */ }

  // Try inside curly braces
  const m = raw.match(/\{[\s\S]*\}/);
  if (m) {
    try {
      const parsed = JSON.parse(m[0]);
      if (parsed.title && parsed.content) {
        return { title: sanitizeTitle(parsed.title), content: sanitizeContent(parsed.content) };
      }
    } catch { /* fall through */ }
  }

  // Last resort: use entire raw text as content (not JSON, just story text)
  console.warn("[story] Non-JSON response. Using raw text.");
  return {
    title: `On this day, July ${row.calendar_date.slice(8)}`,
    content: raw.replace(/^[\{\}\"']/g, "").trim() || raw,
  };
}

// ── Helpers ──

function getSeason(month: number): string {
  if (month === 12 || month <= 2) return "winter";
  if (month <= 5) return "spring";
  if (month <= 8) return "summer";
  return "autumn";
}

/** Convert ISO time string to a human time-of-day label. */
function timeOfDayLabel(isoTime: string): string {
  try {
    const h = new Date(isoTime + "Z").getUTCHours();
    if (h >= 0 && h < 4) return "late night";
    if (h < 6) return "early morning";
    if (h < 8) return "morning";
    if (h < 11) return "late morning";
    if (h < 13) return "noon";
    if (h < 17) return "afternoon";
    if (h < 19) return "evening";
    if (h < 22) return "night";
    return "late night";
  } catch { return ""; }
}

/** Strip obvious JSON artifacts from title. */
function sanitizeTitle(t: string): string {
  return t.replace(/^title:?\s*/i, "").replace(/^["'"]|["'"]$/g, "").trim();
}

/** Strip JSON/formatting artifacts from content. */
function sanitizeContent(c: string): string {
  return c
    .replace(/^content:?\s*/i, "")
    .replace(/\\n/g, "\n")
    .replace(/\\"/g, "\"")
    .trim();
}

/** Nominatim geocoding — free, no key, 1 req/s rate limit. */
async function geocodePlace(place: string): Promise<{ lat: number; lon: number }> {
  const url = `https://nominatim.openstreetmap.org/search?format=json&q=${encodeURIComponent(place)}&limit=1`;
  const res = await fetch(url, { headers: { "User-Agent": "ThatDay/1.0" } });
  if (!res.ok) throw new Error(`Nominatim error: ${res.status}`);
  const data: any[] = await res.json();
  if (!data?.length) throw new Error(`No geocoding result for: ${place}`);
  return { lat: parseFloat(data[0].lat), lon: parseFloat(data[0].lon) };
}

/** Open-Meteo archive API — free, no key, historical weather. */
async function fetchWeather(
  lat: number,
  lon: number,
  date: string
): Promise<string> {
  try {
    const url = `https://archive-api.open-meteo.com/v1/archive?latitude=${lat}&longitude=${lon}&start_date=${date}&end_date=${date}&daily=temperature_2m_max,temperature_2m_min,precipitation_sum,weather_code&timezone=auto`;
    const res = await fetch(url);
    if (!res.ok) return "";
    const data: any = await res.json();
    const daily = data?.daily;
    if (!daily?.weather_code?.length) return "";

    const code = daily.weather_code[0];
    const maxTemp = daily.temperature_2m_max?.[0];
    const minTemp = daily.temperature_2m_min?.[0];
    const precip = daily.precipitation_sum?.[0];

    const desc = weatherDesc(code);
    const parts: string[] = [];
    if (desc) parts.push(desc);
    if (maxTemp != null && minTemp != null) {
      parts.push(`${Math.round(minTemp)}–${Math.round(maxTemp)}°C`);
    }
    if (precip > 0) parts.push(`${precip.toFixed(1)}mm rain`);

    return parts.join(", ");
  } catch {
    return "";
  }
}

/** WMO weather codes → short English description. */
function weatherDesc(code: number): string {
  if (code === 0) return "clear sky";
  if (code <= 3) return "partly cloudy";
  if (code <= 48) return "foggy";
  if (code <= 57) return "drizzle";
  if (code <= 67) return "rain";
  if (code <= 77) return "snow";
  if (code <= 82) return "showers";
  if (code <= 86) return "heavy snow";
  if (code <= 99) return "thunderstorm";
  return "";
}

// ── POST /generate-story ──
// Manual trigger from desktop.
//   Seal Today: generates story + will send email separately via /send-story
//   Save flow:  no_email=true → story generated, status='generated', Cron emails later.

story.post("/generate-story", async (c) => {
  const userId = c.get("userId") as string;
  const { calendar_date, no_email } = await c.req.json<{ calendar_date?: string; no_email?: boolean }>();

  // Find pending/generated/sending_failed annotation for this user, optionally filtered by date
  const row = await c.env.DB.prepare(
    calendar_date
      ? `SELECT * FROM daily_annotations WHERE user_id = ?1 AND status IN ('pending','generated','sending_failed') AND calendar_date = ?2 LIMIT 1`
      : `SELECT * FROM daily_annotations WHERE user_id = ?1 AND status IN ('pending','generated','sending_failed') ORDER BY calendar_date DESC LIMIT 1`
  ).bind(userId, ...(calendar_date ? [calendar_date] : [])).first<AnnotationRow>();

  if (!row) {
    return c.json({ error: "No pending/generated annotations found" }, 404);
  }

  // Mark processing
  await c.env.DB.prepare(
    "UPDATE daily_annotations SET status = 'processing', updated_at = datetime('now') WHERE id = ?1"
  ).bind(row.id).run();

  try {
    const story = await generateStory(c.env, row);

    // Save to stories table
    const storyId = crypto.randomUUID();
    await c.env.DB.prepare(
      `INSERT INTO stories (id, user_id, calendar_date, title, content, photos_json)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6)
       ON CONFLICT(user_id, calendar_date) DO UPDATE SET
         title = excluded.title,
         content = excluded.content,
         photos_json = excluded.photos_json`
    ).bind(storyId, userId, row.calendar_date, story.title, story.content, row.photos_json).run();

    // Mark annotation: 'generated' if no_email (Save flow), 'sent' otherwise (Seal flow)
    const newStatus = no_email ? 'generated' : 'sent';
    await c.env.DB.prepare(
      `UPDATE daily_annotations SET status = ?1, updated_at = datetime('now') WHERE id = ?2`
    ).bind(newStatus, row.id).run();

    return c.json({
      success: true,
      story: {
        calendar_date: row.calendar_date,
        title: story.title,
        content: story.content,
      },
    });
  } catch (err: any) {
    // Only mark failed if currently processing — don't overwrite 'generated' or 'sent'
    await c.env.DB.prepare(
      "UPDATE daily_annotations SET status = 'sending_failed', updated_at = datetime('now') WHERE id = ?1 AND status = 'processing'"
    ).bind(row.id).run();

    console.error("Story generation failed:", err.message);
    return c.json({ error: `Story generation failed: ${err.message}` }, 500);
  }
});

// ── POST /send-story ──
// Manual email send (calls shared sendStoryEmail)

story.post("/send-story", async (c) => {
  const userId = c.get("userId") as string;
  const { calendar_date, audio_url, pdf_url } = await c.req.json<{
    calendar_date?: string;
    audio_url?: string;
    pdf_url?: string;
  }>();

  if (!calendar_date) {
    return c.json({ error: "calendar_date is required" }, 400);
  }

  // Find the story
  const storyRow = await c.env.DB.prepare(
    "SELECT id, title, content, calendar_date FROM stories WHERE user_id = ?1 AND calendar_date = ?2"
  ).bind(userId, calendar_date).first<{
    id: string; title: string | null; content: string; calendar_date: string;
  }>();

  if (!storyRow) {
    return c.json({ error: "Story not found for this date" }, 404);
  }

  const { sendStoryEmail } = await import("./email");
  const success = await sendStoryEmail(c.env as any, userId, storyRow, audio_url, pdf_url);

  if (!success) {
    return c.json({ error: "Failed to send email" }, 500);
  }

  return c.json({ success: true, message: "Story email sent" });
});

// ── GET /stories ──
// List all stories for user

story.get("/stories", async (c) => {
  const userId = c.get("userId") as string;
  const limit = parseInt(c.req.query("limit") || "30");

  const result = await c.env.DB.prepare(
    "SELECT calendar_date, title, content, created_at FROM stories WHERE user_id = ?1 ORDER BY calendar_date DESC LIMIT ?2"
  ).bind(userId, limit).all();

  return c.json({ stories: result.results });
});

// ── GET /stories/:date ──
// Get specific date story

story.get("/stories/:date", async (c) => {
  const userId = c.get("userId") as string;
  const date = c.req.param("date");

  if (!/^\d{4}-\d{2}-\d{2}$/.test(date)) {
    return c.json({ error: "Invalid date format, use YYYY-MM-DD" }, 400);
  }

  try {
    const row = await c.env.DB.prepare(
      "SELECT calendar_date, title, content, created_at FROM stories WHERE user_id = ?1 AND calendar_date = ?2"
    ).bind(userId, date).first();

    if (!row) {
      return c.json({ error: "Story not found" }, 404);
    }

    return c.json({ story: row });
  } catch (err: any) {
    console.error("GET /stories/:date error:", err.message, err.stack);
    return c.json({ error: `Internal error: ${err.message}` }, 500);
  }
});
