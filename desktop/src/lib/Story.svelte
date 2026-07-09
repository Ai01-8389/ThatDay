<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

  let { token: _token, onBack }: { token?: string; onBack: () => void } = $props();

  interface StoryItem {
    calendar_date: string;
    title: string;
    content: string;
    created_at?: string;
  }

  interface StoryPhoto {
    file_path_hash: string;
    file_name: string;
    taken_at: number | null;
    who: string | null;
    where_place: string | null;
    event: string | null;
    scene_tags: string[];
  }

  interface StoryPhotoDisplay {
    hash: string;
    dataUrl: string;
    annotated: boolean;
  }

  let stories = $state<StoryItem[]>([]);
  let loading = $state(true);
  let error = $state("");
  let selected = $state<StoryItem | null>(null);
  let sendMsg = $state("");
  let reading = $state(false);
  let highlightStart = $state(-1);
  let highlightEnd = $state(-1);
  let readingSupported = $state(typeof window !== "undefined" && "speechSynthesis" in window);

  // ── Problem 7: Photos for story detail ──
  let storyPhotos = $state<StoryPhotoDisplay[]>([]);
  let storyPhotosLoading = $state(false);

  $effect(() => {
    loadStories();
    if (readingSupported) initVoices();
  });

  async function loadStories() {
    loading = true;
    error = "";
    try {
      stories = await invoke<StoryItem[]>("get_stories", { limit: 50 });
      // P1: pre-classify next 7 days in background
      preclassifyNearby().catch(() => {});
    } catch (e: any) {
      error = e.message || "Failed to load stories";
    } finally {
      loading = false;
    }
  }

  async function openStory(calendar_date: string) {
    error = "";
    storyPhotos = [];
    try {
      const result = await invoke<StoryItem | null>("get_story_local", { date: calendar_date });
      if (result) {
        selected = result;
        // Load photos for this date (fire-and-forget)
        loadStoryPhotos(calendar_date);
      } else {
        error = "Story not found locally.";
      }
    } catch (e: any) {
      error = e.message || "Failed to load story";
    }
  }

  async function loadStoryPhotos(date: string) {
    storyPhotosLoading = true;
    try {
      // date is "YYYY-MM-DD" from Worker, convert to "MM-DD" for get_photos_by_date
      const md = date.slice(5);
      let raw = await invoke<StoryPhoto[]>("get_photos_by_date", { date: md });

      // Lightweight burst dedup: time window ≤ 10s, keep only one per burst
      raw = raw.sort((a, b) => (a.taken_at || 0) - (b.taken_at || 0));
      const deduped: StoryPhoto[] = [];
      for (const p of raw) {
        const last = deduped[deduped.length - 1];
        if (last && p.taken_at && last.taken_at && p.taken_at - last.taken_at <= 10) continue;
        deduped.push(p);
      }
      raw = deduped;

      // Sort by annotation weight: fully annotated > has who > rest
      const sorted = [...raw].sort((a, b) => {
        const aAll = a.who && a.where_place && a.event ? 0 : a.who ? 1 : 2;
        const bAll = b.who && b.where_place && b.event ? 0 : b.who ? 1 : 2;
        return aAll - bAll;
      });

      // Cap at 20, fill with random if needed
      const top = sorted.slice(0, 20);

      // Load thumbnails as base64
      const display: StoryPhotoDisplay[] = [];
      for (const p of top) {
        try {
          const dataUrl = await invoke<string | null>("read_thumbnail_base64", { hash: p.file_path_hash });
          if (dataUrl) {
            const annotated = !!(p.who && p.where_place && p.event);
            display.push({ hash: p.file_path_hash, dataUrl, annotated });
          }
        } catch {
          // Skip photos whose thumbnails can't be read
        }
      }
      storyPhotos = display;
    } catch (e) {
      console.error("Failed to load story photos:", e);
    } finally {
      storyPhotosLoading = false;
    }
  }

  function backToList() {
    selected = null;
    storyPhotos = [];
    stopReading();
  }

  function formatDate(date: string): string {
    try {
      return new Date(date + "T00:00:00").toLocaleDateString("en-US", {
        weekday: "long",
        year: "numeric",
        month: "long",
        day: "numeric",
      });
    } catch {
      return date;
    }
  }

  // ── P1: idle pre-classification ──

  async function preclassifyNearby() {
    const today = new Date();
    for (let i = 1; i <= 7; i++) {
      const d = new Date(today);
      d.setDate(d.getDate() + i);
      const dateStr = d.toISOString().slice(0, 10);
      try {
        await invoke<number>("classify_date", { date: dateStr });
      } catch {
        // Best-effort, ignore errors
      }
    }
  }

  // ── Read Aloud ──

  let selectedVoice: SpeechSynthesisVoice | null = $state(null);
  let voicesLoaded = $state(false);

  /** Pick best voice, with debug logging. */
  function pickBest(): SpeechSynthesisVoice | null {
    if (!readingSupported) return null;
    const all = window.speechSynthesis.getVoices();
    if (all.length === 0) return null;

    // Log once for debugging
    if (!voicesLoaded) {
      console.log(`[tts-web] ${all.length} Web Speech voices:`);
      for (const v of all) console.log(`[tts-web]   ${v.name} (${v.lang}) ${v.default ? '[default]' : ''}`);
      voicesLoaded = true;
    }

    const en = all.filter(v => v.lang.startsWith("en"));
    if (en.length === 0) return null;

    // Score: lower = better. Prefer female voices.
    const score = (v: SpeechSynthesisVoice): number => {
      const n = v.name.toLowerCase();
      if (n.includes("aria") || n.includes("jenny") || n.includes("natasha")) return 0;   // female neural
      if ((n.includes("samantha") || n.includes("hazel") || n.includes("susan")) && !n.includes("zira")) return 1; // macOS/Win female
      if (n.includes("zira")) return 2;    // default female on Windows
      if (n.includes("david") || n.includes("mark") || n.includes("guy") || n.includes("ryan")) return 5; // male
      if (n.includes("microsoft")) return 6;
      return 7;
    };
    en.sort((a, b) => score(a) - score(b));
    console.log(`[tts-web] Selected: ${en[0].name}`);
    return en[0];
  }

  /** Load voices. Called early via $effect, but voices may load async.
   *  WebView2 sometimes needs a user gesture or a forced load. */
  function initVoices() {
    // Force-load: calling getVoices() in some browsers triggers a load
    const v = pickBest();
    if (v) {
      selectedVoice = v;
      return;
    }
    // Listen for async load
    const handler = () => {
      const best = pickBest();
      if (best) selectedVoice = best;
      speechSynthesis.removeEventListener("voiceschanged", handler);
    };
    speechSynthesis.addEventListener("voiceschanged", handler);
    // Some browsers need an extra kick
    setTimeout(() => {
      if (!selectedVoice) {
        const best = pickBest();
        if (best) selectedVoice = best;
      }
    }, 500);
  }

  function startReading(text: string) {
    if (!readingSupported) return;
    window.speechSynthesis.cancel();
    highlightStart = -1;
    highlightEnd = -1;
    const u = new SpeechSynthesisUtterance(text);
    u.lang = "en-US";
    u.rate = 0.9;
    u.pitch = 1.0;

    // Try to pick voice now if not already set
    if (!selectedVoice) initVoices();
    if (selectedVoice) {
      u.voice = selectedVoice;
      console.log(`[tts-web] Using voice: ${selectedVoice.name}`);
    } else {
      console.log("[tts-web] No voice selected, using browser default");
    }
    u.onboundary = (event: any) => {
      if (event.name === "sentence" || event.charIndex !== undefined) {
        const ci: number = event.charIndex ?? event.charIndex;
        if (ci >= 0 && ci < text.length) {
          let s = ci;
          let e = ci;
          while (s > 0 && !/[.!?]\s*$/.test(text.slice(0, s))) s--;
          while (s > 0 && /[.!?]/.test(text[s - 1])) s--;
          while (e < text.length && !/[.!?]/.test(text[e])) e++;
          if (e < text.length) e++; // include punctuation
          highlightStart = s;
          highlightEnd = e;
        }
      }
    };
    u.onend = () => { reading = false; highlightStart = -1; highlightEnd = -1; };
    u.onerror = () => { reading = false; highlightStart = -1; highlightEnd = -1; };
    reading = true;
    window.speechSynthesis.speak(u);
  }

  function stopReading() {
    reading = false;
    window.speechSynthesis.cancel();
  }

  // Voice display name for the button tooltip
  let voiceLabel = $derived(
    selectedVoice ? (selectedVoice as SpeechSynthesisVoice).name.replace("Microsoft ", "").replace(" - English (United Kingdom)", "") : "default"
  );

  function toggleReading(text: string) {
    if (reading) {
      stopReading();
    } else {
      startReading(text);
    }
  }

  // ── PDF: open existing or open save folder ──
  let generatingPdf = $state(false);

  async function handleGeneratePdf(calendar_date: string) {
    generatingPdf = true;
    sendMsg = "";
    try {
      // First, generate the PDF (idempotent: skips if already exists)
      const story = selected;
      if (!story) return;
      try {
        await invoke("generate_pdf", { date: calendar_date, title: story.title, content: story.content });
      } catch (e: any) {
        console.error("PDF generation failed:", e);
        sendMsg = `Generation failed: ${e.message || e}`;
        generatingPdf = false;
        return;
      }

      // Now get the path and open it
      const pdfPath = await invoke<string>("get_pdf_path", { date: calendar_date });
      try {
        await invoke("open_path", { path: pdfPath });
        sendMsg = `Opened ${calendar_date}.pdf`;
      } catch {
        const parent = pdfPath.replace(/\\[^\\]+$/, "");
        await invoke("open_path", { path: parent });
        sendMsg = "Folder opened. PDF may not have been generated.";
      }
    } catch (e: any) {
      console.error("PDF open failed:", e);
      sendMsg = `Failed: ${e.message || e}`;
    } finally {
      generatingPdf = false;
    }
  }
</script>

<div class="story-page">
  <header>
    <button class="back-btn" onclick={selected ? backToList : onBack}>
      ← {selected ? "Back to list" : "Back"}
    </button>
    <h1>{selected ? selected.title : "Your Stories"}</h1>
    <span class="spacer" />
  </header>

  <main>
    {#if loading}
      <div class="center"><p class="muted">Loading stories…</p></div>

    {:else if error && !selected}
      <div class="center">
        <p class="error">{error}</p>
        <button class="retry-btn" onclick={loadStories}>Retry</button>
      </div>

    {:else if selected}
      <!-- ── Story Detail ── -->
      <article class="story-detail">
        <div class="story-meta">
          <span class="story-date">{formatDate(selected.calendar_date)}</span>
        </div>
        <div class="story-body">
          {#if reading && highlightStart >= 0 && highlightEnd > highlightStart}
            <p>
              {selected.content.slice(0, highlightStart)}<mark>{selected.content.slice(highlightStart, highlightEnd)}</mark>{selected.content.slice(highlightEnd)}
            </p>
          {:else}
            {@html selected.content.split("\n").filter(p => p.trim()).map(p => `<p>${p}</p>`).join("")}
          {/if}
        </div>
        <div class="story-actions">
          {#if readingSupported}
            <button
              class="action-btn {reading ? 'active' : ''}"
              onclick={() => toggleReading(selected!.content)}
              title="Voice: {voiceLabel}"
            >
              {reading ? "■ Stop" : "▶ Read Aloud"}
            </button>
          {/if}
          <button
            class="action-btn"
            disabled={generatingPdf}
            onclick={() => handleGeneratePdf(selected!.calendar_date)}
          >
            {generatingPdf ? "Generating…" : "📄 PDF"}
          </button>
          {#if sendMsg}
            <span class="send-msg">{sendMsg}</span>
          {/if}
        </div>

        <!-- ── Problem 7: Photos from this day ── -->
        {#if storyPhotosLoading}
          <div class="photo-strip">
            <div class="strip-label">Photos from this day</div>
            <p class="strip-loading">Loading photos…</p>
          </div>
        {:else if storyPhotos.length > 0}
          <div class="photo-strip">
            <div class="strip-label">Photos from this day ({storyPhotos.length})</div>
            <div class="strip-scroll">
              {#each storyPhotos as sp}
                <div class="strip-thumb" class:annotated={sp.annotated}>
                  <img src={sp.dataUrl} alt="" loading="lazy" />
                </div>
              {/each}
            </div>
          </div>
        {/if}
      </article>

    {:else if stories.length === 0}
      <div class="center">
        <p class="muted">No stories yet.</p>
        <p class="sub">Annotate and seal a day to generate your first story.</p>
      </div>

    {:else}
      <!-- ── Story List ── -->
      <div class="story-list">
        {#each stories as s}
          <button class="story-card" onclick={() => openStory(s.calendar_date)}>
            <span class="card-date">{formatDate(s.calendar_date)}</span>
            <span class="card-title">{s.title}</span>
            <span class="card-excerpt">{s.content.slice(0, 120)}…</span>
          </button>
        {/each}
      </div>
    {/if}
  </main>
</div>

<style>
  .story-page {
    display: flex;
    flex-direction: column;
    min-height: 100vh;
    font-family: system-ui, -apple-system, sans-serif;
    background: #faf9f7;
  }

  header {
    display: flex;
    align-items: center;
    gap: 1rem;
    padding: 1rem 1.5rem;
    background: white;
    border-bottom: 1px solid #e5e7eb;
    position: sticky;
    top: 0;
    z-index: 10;
  }
  header h1 {
    font-size: 1.15rem;
    font-weight: 600;
    color: #1f2937;
    margin: 0;
    flex: 1;
    text-align: center;
  }
  .spacer { width: 80px; }

  .back-btn {
    padding: 0.4rem 0.9rem;
    border: 1px solid #d1d5db;
    border-radius: 8px;
    background: white;
    font-size: 0.85rem;
    color: #374151;
    cursor: pointer;
    transition: background 0.15s;
    white-space: nowrap;
  }
  .back-btn:hover { background: #f3f4f6; }

  main {
    flex: 1;
    padding: 1.5rem;
    max-width: 720px;
    margin: 0 auto;
    width: 100%;
    box-sizing: border-box;
  }

  .center {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 4rem 1rem;
    text-align: center;
  }
  .muted { color: #9ca3af; font-size: 1rem; }
  .sub { color: #c4c4c4; font-size: 0.85rem; margin-top: 0.5rem; }
  .error { color: #ef4444; font-size: 0.9rem; margin-bottom: 1rem; }
  .retry-btn {
    padding: 0.5rem 1.5rem;
    border: 1px solid #d1d5db;
    border-radius: 8px;
    background: white;
    cursor: pointer;
  }

  /* ── List ── */
  .story-list {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  .story-card {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    padding: 1rem 1.25rem;
    background: white;
    border: 1px solid #e5e7eb;
    border-radius: 10px;
    text-align: left;
    cursor: pointer;
    transition: box-shadow 0.15s, border-color 0.15s;
  }
  .story-card:hover {
    border-color: #c7d2fe;
    box-shadow: 0 2px 12px rgba(99, 102, 241, 0.08);
  }
  .card-date {
    font-size: 0.75rem;
    color: #9ca3af;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .card-title {
    font-size: 1.05rem;
    font-weight: 600;
    color: #1f2937;
  }
  .card-excerpt {
    font-size: 0.85rem;
    color: #6b7280;
    line-height: 1.5;
  }

  /* ── Detail ── */
  .story-detail {
    background: white;
    border: 1px solid #e5e7eb;
    border-radius: 12px;
    padding: 2rem;
  }
  .story-meta {
    margin-bottom: 1.5rem;
  }
  .story-date {
    font-size: 0.8rem;
    color: #9ca3af;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .story-body {
    font-size: 1.05rem;
    line-height: 1.85;
    color: #374151;
    margin-bottom: 2rem;
  }
  .story-body :global(p) {
    margin: 0 0 0.8rem;
  }
  .story-body :global(mark) {
    background: #fef08a;
    color: #1f2937;
    padding: 0 2px;
    border-radius: 2px;
  }

  /* ── Actions ── */
  .story-actions {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding-top: 1.25rem;
    border-top: 1px solid #f3f4f6;
  }
  .action-btn {
    padding: 0.5rem 1rem;
    border: 1px solid #d1d5db;
    border-radius: 8px;
    background: white;
    font-size: 0.85rem;
    color: #374151;
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s;
  }
  .action-btn:hover:not(:disabled) { background: #f9fafb; border-color: #9ca3af; }
  .action-btn:disabled { opacity: 0.5; cursor: not-allowed; }
  .action-btn.active {
    background: #eef2ff;
    border-color: #6366f1;
    color: #4338ca;
  }
  .action-btn.hifi-active {
    background: #fef3c7;
    border-color: #d97706;
    color: #92400e;
  }
  .send-msg {
    font-size: 0.8rem;
    color: #059669;
  }

  /* ── Photo strip ── */
  .photo-strip {
    margin-top: 1.5rem;
    padding-top: 1.25rem;
    border-top: 1px solid #f3f4f6;
  }
  .strip-label {
    font-size: 0.75rem;
    font-weight: 600;
    color: #9ca3af;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    margin-bottom: 0.75rem;
  }
  .strip-loading {
    font-size: 0.85rem;
    color: #c4c4c4;
    margin: 0;
  }
  .strip-scroll {
    display: flex;
    gap: 0.5rem;
    overflow-x: auto;
    padding-bottom: 0.5rem;
    scroll-behavior: smooth;
  }
  .strip-scroll::-webkit-scrollbar {
    height: 4px;
  }
  .strip-scroll::-webkit-scrollbar-thumb {
    background: #d1d5db;
    border-radius: 2px;
  }
  .strip-thumb {
    flex-shrink: 0;
    width: 120px;
    height: 80px;
    border-radius: 8px;
    overflow: hidden;
    border: 2px solid #e5e7eb;
    background: #f3f4f6;
    transition: transform 0.15s, border-color 0.15s;
  }
  .strip-thumb:hover {
    transform: scale(1.05);
    border-color: #93c5fd;
  }
  .strip-thumb.annotated {
    border-color: #3b82f6;
  }
  .strip-thumb img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
</style>
