import type { D1Database, R2Bucket } from "@cloudflare/workers-types";

// Shared environment bindings for every route (main app + sub-routers).
// Keeping a single source of truth lets `tsc --noEmit` actually type-check
// `c.env.*` access (and catch typos like calling a non-existent method).
export interface Env {
  DB: D1Database;
  R2: R2Bucket;
  JWT_SECRET: string;
  MAILTRAP_API_KEY?: string;
  DEEPSEEK_API_KEY?: string;
  CREEM_WEBHOOK_SECRET?: string;
  SUPER_ACCOUNTS?: string;
}

// Variables injected by authMiddleware on every protected route.
export type Variables = {
  userId: string;
  userEmail: string;
  userTier: string;
};

// `@cloudflare/workers-types@4` is missing this runtime method on R2Bucket.
// Augment the type so tsc stops reporting a false error.
declare module "@cloudflare/workers-types" {
  interface R2Bucket {
    createSignedUrl(opts: {
      key: string;
      method?: string;
      signedHeaders?: Headers;
      expiresIn: number;
    }): Promise<string>;
  }
}
