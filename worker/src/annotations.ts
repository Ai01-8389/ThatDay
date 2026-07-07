import { Hono } from "hono";
import type { D1Database } from "@cloudflare/workers-types";

interface AnnotationsBindings {
  DB: D1Database;
  DEEPSEEK_API_KEY?: string;
}

export interface PhotoAnnotation {
  time: string;          // ISO timestamp
  gps?: { lat: number; lon: number } | null;
  who?: string;
  where?: string;
  event?: string;
  scene_tags?: Record<string, boolean>;  // ONNX 6 binary heads
  season?: string;       // EXIF derived
  time_of_day?: string;  // EXIF derived
}

export const annotations = new Hono<{ Bindings: AnnotationsBindings }>();

// ── PUT /annotations ──
// Receives daily annotations from desktop (upsert, one row per user per day)

annotations.put("/", async (c) => {
  const userId = c.get("userId") as string;
  const { calendar_date, photos } = await c.req.json<{
    calendar_date?: string;
    photos?: PhotoAnnotation[];
  }>();

  if (!calendar_date || !photos || !Array.isArray(photos)) {
    return c.json({ error: "calendar_date and photos array are required" }, 400);
  }

  if (!/^\d{4}-\d{2}-\d{2}$/.test(calendar_date)) {
    return c.json({ error: "calendar_date must be YYYY-MM-DD" }, 400);
  }

  const id = crypto.randomUUID();
  const photosJson = JSON.stringify(photos);

  await c.env.DB.prepare(
    `INSERT INTO daily_annotations (id, user_id, calendar_date, photos_json, status)
     VALUES (?1, ?2, ?3, ?4, 'pending')
     ON CONFLICT(user_id, calendar_date) DO UPDATE SET
       photos_json = excluded.photos_json,
       status = 'pending',
       updated_at = datetime('now')`
  ).bind(id, userId, calendar_date, photosJson).run();

  return c.json({ success: true, calendar_date, photo_count: photos.length });
});

// ── POST /annotations/parse ──
// Natural language → DeepSeek extracts who/where/event

annotations.post("/parse", async (c) => {
  const { text } = await c.req.json<{ text?: string }>();
  if (!text || text.trim().length === 0) {
    return c.json({ error: "text is required" }, 400);
  }

  const apiKey = c.env.DEEPSEEK_API_KEY;
  if (!apiKey) {
    return c.json({ error: "DeepSeek API key not configured" }, 500);
  }

  const systemPrompt = `Extract from natural language text: who (people names), where (place), event (activity).
Return ONLY valid JSON: {"who": "...", "where": "...", "event": "..."}
If a field is not mentioned, use empty string.
Keep values short (under 60 characters each).`;

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
        { role: "user", content: text.trim() },
      ],
      max_tokens: 100,
      temperature: 0.1,
      response_format: { type: "json_object" },
    }),
  });

  if (!response.ok) {
    const errText = await response.text();
    console.error("DeepSeek parse error:", errText);
    return c.json({ error: "AI parsing failed" }, 502);
  }

  const data: any = await response.json();
  const content = data.choices?.[0]?.message?.content || "{}";

  try {
    const parsed = JSON.parse(content);
    return c.json({
      who: parsed.who || "",
      where: parsed.where || "",
      event: parsed.event || "",
    });
  } catch {
    return c.json({ who: "", where: "", event: "" });
  }
});
