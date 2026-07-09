import { Hono } from "hono";
import type { Env } from "./types";

interface WebhookPayload {
  type: string;
  data: {
    object: {
      customer?: { email?: string };
      status?: string;
    };
  };
}

export const payment = new Hono<{ Bindings: Env }>();

/**
 * POST /payment/webhook — Creem payment events
 * HMAC-SHA256 signature verification.
 * Handles: subscription.active | subscription.canceled | subscription.past_due | subscription.expired
 */
payment.post("/webhook", async (c) => {
  const signature = c.req.header("creem-signature");
  if (!signature) {
    return c.json({ error: "Missing signature" }, 401);
  }

  const secret = c.env.CREEM_WEBHOOK_SECRET;
  if (!secret) {
    console.error("CREEM_WEBHOOK_SECRET not configured");
    return c.json({ error: "Webhook not configured" }, 500);
  }

  const body = await c.req.text();

  // Verify HMAC
  const encoder = new TextEncoder();
  const key = await crypto.subtle.importKey(
    "raw", encoder.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false, ["verify"]
  );

  const sigParts = signature.split(",");
  const tPart = sigParts.find((p: string) => p.startsWith("t="));
  const vPart = sigParts.find((p: string) => p.startsWith("v1="));
  if (!tPart || !vPart) {
    return c.json({ error: "Invalid signature format" }, 401);
  }

  const timestamp = tPart.slice(2);
  const providedHex = vPart.slice(3);
  const signedPayload = `${timestamp}.${body}`;
  const sigBytes = Uint8Array.from(
    providedHex.match(/.{1,2}/g)!.map((b: string) => parseInt(b, 16))
  );

  const valid = await crypto.subtle.verify(
    "HMAC", key, sigBytes, encoder.encode(signedPayload)
  );
  if (!valid) {
    return c.json({ error: "Invalid signature" }, 401);
  }

  // Parse payload
  let payload: WebhookPayload;
  try { payload = JSON.parse(body); } catch {
    return c.json({ error: "Invalid JSON" }, 400);
  }

  const email = payload.data.object.customer?.email;
  if (!email) {
    console.warn("[payment] No customer email in webhook payload");
    return c.json({ received: true });
  }
  const normalized = email.toLowerCase().trim();

  switch (payload.type) {
    // ── Upgrade ──
    // Use UPSERT: user may not exist yet (subscribed via web before installing desktop app)
    case "subscription.active":
      await c.env.DB.prepare(
        `INSERT INTO users (id, email, tier, trial_start, trial_end) VALUES (?1, ?2, 'pro', NULL, NULL)
         ON CONFLICT(email) DO UPDATE SET tier = 'pro', trial_start = NULL, trial_end = NULL`
      ).bind(crypto.randomUUID(), normalized).run();
      console.log(`[payment] Upgraded to pro: ${normalized}`);
      break;

    // ── Downgrade ──
    case "subscription.canceled":
    case "subscription.expired":
      // Only downgrade if user already exists (skip if they never registered)
      await c.env.DB.prepare(
        "UPDATE users SET tier = 'free' WHERE email = ?1 AND tier = 'pro'"
      ).bind(normalized).run();
      console.log(`[payment] Downgraded to free: ${normalized} (${payload.type})`);
      break;

    // ── Payment issue (log only, don't revoke yet) ──
    case "subscription.past_due":
      console.warn(`[payment] Payment past due: ${normalized}`);
      break;

    default:
      console.log(`[payment] Unhandled event: ${payload.type}`);
  }

  return c.json({ received: true });
});
