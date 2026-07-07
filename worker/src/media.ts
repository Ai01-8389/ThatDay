import { Hono } from "hono";
import type { R2Bucket, R2Object } from "@cloudflare/workers-types";
import { authMiddleware } from "./auth";

interface MediaBindings {
  R2: R2Bucket;
}

export const media = new Hono<{ Bindings: MediaBindings }>();

// ── POST /media/upload ──
// Upload PDF + WAV for a date. JWT required.
// Multipart fields: "pdf" (application/pdf), "audio" (audio/wav), "date" (text)

media.post("/upload", authMiddleware, async (c) => {
  const userId = c.get("userId") as string;
  const form = await c.req.formData();

  const date = form.get("date") as string;
  const pdf = form.get("pdf") as File | null;
  const audio = form.get("audio") as File | null;

  if (!date) return c.json({ error: "Missing date field" }, 400);

  const uploaded: Record<string, string> = {};

  // Store PDF
  if (pdf && pdf.size > 0) {
    if (pdf.size > 10 * 1024 * 1024) {
      return c.json({ error: "PDF exceeds 10MB limit" }, 413);
    }
    const pdfKey = `${userId}/${date}.pdf`;
    await c.env.R2.put(pdfKey, await pdf.arrayBuffer(), {
      httpMetadata: { contentType: "application/pdf" },
    });
    uploaded.pdf = pdfKey;
  }

  // Store audio
  if (audio && audio.size > 0) {
    const wavKey = `${userId}/${date}.wav`;
    await c.env.R2.put(wavKey, await audio.arrayBuffer(), {
      httpMetadata: { contentType: "audio/wav" },
    });
    uploaded.audio = wavKey;
  }

  const result: Record<string, string> = {};
  if (uploaded.pdf) result.pdf_url = `https://api.thatday.vip/pdf/${userId}/${date}`;
  if (uploaded.audio) result.audio_url = `https://api.thatday.vip/audio/${userId}/${date}`;

  console.log(`[media] User ${userId} uploaded ${Object.keys(uploaded).join(", ")} for ${date}`);
  return c.json(result);
});

// ── GET /pdf/:uid/:date ──
// Proxy to R2 presigned URL (1h TTL)

media.get("/pdf/:uid/:date", async (c) => {
  const uid = c.req.param("uid");
  const date = c.req.param("date");
  return proxyR2(c, uid, date, "pdf");
});

// ── GET /audio/:uid/:date ──
// Proxy to R2 presigned URL (1h TTL)

media.get("/audio/:uid/:date", async (c) => {
  const uid = c.req.param("uid");
  const date = c.req.param("date");
  return proxyR2(c, uid, date, "wav");
});

async function proxyR2(c: any, uid: string, date: string, ext: string) {
  const key = `${uid}/${date}.${ext}`;

  // Check if object exists
  const head = await c.env.R2.head(key);
  if (!head) {
    return c.html(
      `<html><body style="font-family:sans-serif;text-align:center;padding:60px 20px;color:#6b7280">
        <h1 style="font-size:48px;margin:0">⏳</h1>
        <p style="font-size:18px;margin:16px 0">This ${ext === "pdf" ? "keepsake" : "audio"} has expired.</p>
        <p style="font-size:14px">Open That Day app and seal this date again for a fresh link.</p>
        <p style="font-size:12px;color:#9ca3af;margin-top:24px">That Day · thatday.vip</p>
      </body></html>`,
      410
    );
  }

  // Generate signed URL (1 hour TTL)
  try {
    const signedUrl = await c.env.R2.createSignedUrl({
      key,
      method: "GET",
      signedHeaders: new Headers(),
      expiresIn: 3600,
    });
    return c.redirect(signedUrl, 302);
  } catch (err: any) {
    console.error(`[media] Signed URL error for ${key}:`, err?.message);
    // Fallback: serve directly (limited to free tier bandwidth)
    const obj = await c.env.R2.get(key);
    if (!obj) return c.text("Not found", 404);
    c.header("Content-Type", obj.httpMetadata?.contentType || "application/octet-stream");
    c.header("Cache-Control", "public, max-age=3600");
    return c.body(obj.body);
  }
}
