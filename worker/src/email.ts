import type { Env } from "./types";

interface StoryRow {
  id: string;
  title: string | null;
  content: string;
  calendar_date: string;
}

/**
 * Base64URL encode (RFC 4648 §5): replaces +/ with -_, strips =
 */
function base64urlEncode(str: string): string {
  const base64 = btoa(unescape(encodeURIComponent(str)));
  return base64.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * Send the daily story email via Mailtrap.
 * Called by both manual /send-story and Cron.
 */
export async function sendStoryEmail(
  env: Env,
  userId: string,
  story: StoryRow,
  audioUrl?: string,
  pdfUrl?: string
): Promise<boolean> {
  if (!env.MAILTRAP_API_KEY) {
    console.error("MAILTRAP_API_KEY not configured");
    return false;
  }

  // Get user email
  const user = await env.DB.prepare(
    "SELECT email FROM users WHERE id = ?1"
  ).bind(userId).first<{ email: string }>();

  if (!user) {
    console.error("User not found:", userId);
    return false;
  }

  // Build listen URL: thatday.vip/listen#base64url(title|content)
  const hashPayload = `${story.title || "Your story"}|${story.content}`;
  const listenUrl = `https://thatday.vip/listen#${base64urlEncode(hashPayload)}`;

  // Story preview: first 200 characters
  const preview = story.content.slice(0, 200).replace(/\n/g, " ") +
    (story.content.length > 200 ? "…" : "");

  // Format date
  const dateDisplay = story.calendar_date;

  const html = [
    `<p>${preview}</p>`,
    `<p style="margin:24px 0">`,
    audioUrl
      ? `  <a href="${audioUrl}" style="display:inline-block;padding:12px 24px;background:#4F46E5;color:white;text-decoration:none;border-radius:8px;font-weight:600">🎧 Listen to your story</a>`
      : "",
    pdfUrl
      ? `&nbsp;&nbsp;<a href="${pdfUrl}" style="display:inline-block;padding:12px 24px;background:#059669;color:white;text-decoration:none;border-radius:8px;font-weight:600">📄 Download keepsake</a>`
      : "",
    `</p>`,
    audioUrl ? "" : `<p style="margin:24px 0"><a href="${listenUrl}" style="display:inline-block;padding:12px 24px;background:#4F46E5;color:white;text-decoration:none;border-radius:8px;font-weight:600">🎧 Listen to your story</a></p>`,
    `<hr style="border:none;border-top:1px solid #e5e7eb;margin:32px 0">`,
    `<p style="color:#9ca3af;font-size:12px">`,
    `That Day — Every day deserves to be remembered<br>`,
    `This link contains your personal story. Please don't forward.`,
    `</p>`,
  ].join("\n");

  const res = await fetch("https://send.api.mailtrap.io/api/send", {
    method: "POST",
    headers: {
      Authorization: "Bearer " + env.MAILTRAP_API_KEY,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      from: { email: "hello@thatday.vip", name: "That Day" },
      to: [{ email: user.email }],
      subject: `Your story for ${dateDisplay} is ready ✨`,
      html,
      category: "story",
    }),
  });

  if (!res.ok) {
    const errText = await res.text();
    console.error("Mailtrap send error:", errText);
    return false;
  }

  return true;
}
