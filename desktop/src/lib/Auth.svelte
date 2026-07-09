<script lang="ts">
  import { requestOtp, verifyOtp } from "./api";
  import { invoke } from "@tauri-apps/api/core";

  let { onLogin }: { onLogin: (e: { token: string; email: string; tier: string; trial_active?: boolean }) => void } = $props();

  let email = $state("");
  let code = $state("");
  let step = $state<"email" | "code">("email");
  let sending = $state(false);
  let verifying = $state(false);
  let error = $state("");

  async function sendCode() {
    if (!email || !email.includes("@")) {
      error = "Please enter a valid email";
      return;
    }
    error = "";
    sending = true;
    try {
      await requestOtp(email);
      step = "code";
    } catch (err: any) {
      error = err.message || "Something went wrong";
    } finally {
      sending = false;
    }
  }

  async function verifyCode() {
    if (!code || code.length !== 6) {
      error = "Please enter the 6-digit code";
      return;
    }
    error = "";
    verifying = true;
    try {
      // Detect user's timezone offset (negate because getTimezoneOffset returns inverted sign)
      const utc_offset = -new Date().getTimezoneOffset();
      const data = await verifyOtp(email, code, utc_offset);
      // Persist token to local SQLite so Rust commands (sync, seal) can read it
      invoke("save_auth_token", { token: data.token }).catch(() => {});
      onLogin({ token: data.token, email: data.user.email, tier: data.user.tier, trial_active: data.user.trial_active });
    } catch (err: any) {
      error = err.message || "Invalid code";
    } finally {
      verifying = false;
    }
  }
</script>

<div class="auth-container">
  <h1>That Day</h1>
  <p>Every day deserves to be remembered</p>

  {#if step === "email"}
    <div class="form">
      <label for="email">Email</label>
      <input
        id="email"
        type="email"
        bind:value={email}
        placeholder="you@example.com"
        disabled={sending}
        onkeydown={(e) => e.key === "Enter" && sendCode()}
      />
      <button onclick={sendCode} disabled={sending}>
        {sending ? "Sending..." : "Send Code"}
      </button>
    </div>
  {:else}
    <div class="form">
      <p class="hint">Enter the 6-digit code sent to {email}</p>
      <input
        id="code"
        type="text"
        maxlength="6"
        bind:value={code}
        placeholder="000000"
        disabled={verifying}
        onkeydown={(e) => e.key === "Enter" && verifyCode()}
        oninput={() => { if (code.length === 6) verifyCode(); }}
      />
      <button onclick={verifyCode} disabled={verifying || code.length !== 6}>
        {verifying ? "Verifying..." : "Verify"}
      </button>
      <button class="link" onclick={() => { step = "email"; code = ""; error = ""; }}>
        ← Back
      </button>
    </div>
  {/if}

  {#if error}
    <p class="error">{error}</p>
  {/if}
</div>

<style>
  .auth-container {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    min-height: 100vh;
    padding: 2rem;
    text-align: center;
    font-family: system-ui, -apple-system, sans-serif;
  }
  h1 { font-size: 2rem; color: #1f2937; margin: 0; }
  p { color: #6b7280; margin: 0.5rem 0 2rem; }
  .form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    width: 100%;
    max-width: 320px;
  }
  label { font-weight: 600; text-align: left; color: #374151; }
  input {
    padding: 0.75rem;
    border: 1px solid #d1d5db;
    border-radius: 8px;
    font-size: 1rem;
    text-align: center;
    letter-spacing: 0.25em;
  }
  button {
    padding: 0.75rem;
    background: #4F46E5;
    color: white;
    border: none;
    border-radius: 8px;
    font-size: 1rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.2s;
  }
  button:disabled { opacity: 0.6; cursor: not-allowed; }
  button.link {
    background: none;
    color: #6b7280;
    font-weight: 400;
    text-decoration: underline;
  }
  .hint { font-size: 0.875rem; color: #6b7280; }
  .error { color: #ef4444; font-size: 0.875rem; margin-top: 1rem; }
</style>
