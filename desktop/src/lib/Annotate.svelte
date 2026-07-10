<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { open } from "@tauri-apps/plugin-dialog";
  import { generateStory, getStory, parseAnnotation } from "./api";

  let { token, onStory, onSettings }: {
    token: string;
    onStory: () => void;
    onSettings: () => void;
  } = $props();

  interface Photo {
    file_path_hash: string;
    file_name: string;
    file_path: string;
    file_size: number | null;
    taken_at: number | null;
    gps_lat: number | null;
    gps_lon: number | null;
    thumbnail_path: string | null;
    scene_tags: string[];
    timestamp_source: string | null;
    who: string | null;
    where_place: string | null;
    event: string | null;
    sync_status: string | null;
  }

  interface Annotation {
    file_path_hash: string;
    who: string;
    where_place: string;
    event: string;
  }

  let photos: Photo[] = $state([]);
  let annotations = $state<Record<string, Annotation>>({});
  let loading = $state(true);
  let savedDates = $state<Record<string, boolean>>(loadSavedDates());
  let isSaved = $derived.by(() => savedDates[currentDate]);
  let syncing = $state(false);
  let saveMessage = $state("");
  let scanning = $state(false);
  let scanStatus = $state("");
  let scanDropdownOpen = $state(false);
  let datePickerOpen = $state(false);
  let today = $state(new Date().toISOString().slice(0, 10));
  let todayMD = $derived(today.slice(5)); // "06-30" from "2026-06-30"
  let currentDate = $state(todayMD);
  let availableDates: string[] = $state([]);
  let dateIndex = $state(-1);
  let dateFallback = $state(false);
  let yearRange = $derived(`2015 — ${new Date().getFullYear()}`);
  function computeMaxFuture(md: string): string {
    const [m, d] = md.split('-').map(Number);
    const base = new Date(2000, m - 1, d);
    base.setDate(base.getDate() + 6);
    const mm = String(base.getMonth() + 1).padStart(2, '0');
    const dd = String(base.getDate()).padStart(2, '0');
    return `${mm}-${dd}`;
  }
  let maxFutureMD = $derived(computeMaxFuture(todayMD));
  let selectedPhoto = $state<string | null>(null);
  let editingField = $state<{ photo: string; field: string } | null>(null);

  // ── Produced dates (dates that have been "lived through") ──
  // Each new day on first open is added. Once produced, the date's content is frozen.
  // Only produced dates appear in the picker.
  let producedMDs = $state<string[]>(loadProduced());
  let pickerDates = $derived(producedMDs);

  function loadProduced(): string[] {
    try {
      const raw = localStorage.getItem("thatday_produced_dates");
      return raw ? JSON.parse(raw) : [];
    } catch { return []; }
  }

  function saveProduced() {
    localStorage.setItem("thatday_produced_dates", JSON.stringify(producedMDs));
  }

  // ── Saved dates persistence ──
  function loadSavedDates(): Record<string, boolean> {
    try {
      const raw = localStorage.getItem("thatday_saved_dates");
      return raw ? JSON.parse(raw) : {};
    } catch { return {}; }
  }

  function persistSavedDates() {
    localStorage.setItem("thatday_saved_dates", JSON.stringify(savedDates));
  }

  function produceToday() {
    if (!producedMDs.includes(todayMD)) {
      producedMDs = [...producedMDs, todayMD].sort();
      saveProduced();
    }
  }

  // ── Photo borrowing system (fallback when today has no photos) ──
  // Borrowed photos are banned for 3 months to prevent repetition.
  type BorrowedMap = Record<string, string>; // hash → unban date (YYYY-MM-DD)

  function getBorrowedPhotos(): BorrowedMap {
    try {
      const raw = localStorage.getItem("thatday_borrowed");
      if (!raw) return {};
      const map: BorrowedMap = JSON.parse(raw);
      // Clean expired entries (>3 months old)
      const now = new Date().toISOString().slice(0, 10);
      const cleaned: BorrowedMap = {};
      let changed = false;
      for (const [hash, unbanDate] of Object.entries(map)) {
        if (unbanDate > now) {
          cleaned[hash] = unbanDate;
        } else {
          changed = true;
        }
      }
      if (changed) {
        localStorage.setItem("thatday_borrowed", JSON.stringify(cleaned));
      }
      return cleaned;
    } catch { return {}; }
  }

  function isPhotoBanned(hash: string): boolean {
    const map = getBorrowedPhotos();
    return hash in map;
  }

  function markPhotosBorrowed(hashes: string[]) {
    const unbanDate = new Date();
    unbanDate.setMonth(unbanDate.getMonth() + 3);
    const unbanStr = unbanDate.toISOString().slice(0, 10);
    const map = getBorrowedPhotos();
    for (const h of hashes) {
      map[h] = unbanStr;
    }
    localStorage.setItem("thatday_borrowed", JSON.stringify(map));
  }

  // ── MM-DD helpers ──
  function formatMonthDay(md: string, style: "long" | "short" = "long"): string {
    const [m, d] = md.split('-').map(Number);
    const date = new Date(2000, m - 1, d);
    if (style === "short") {
      return date.toLocaleDateString("en-US", { month: "short", day: "numeric" });
    }
    return date.toLocaleDateString("en-US", { weekday: "long", month: "long", day: "numeric" });
  }

  function monthDayToDOY(md: string): number {
    const [m, d] = md.split('-').map(Number);
    const daysInMonth = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let doy = d;
    for (let i = 1; i < m; i++) doy += daysInMonth[i];
    return doy;
  }

  /** Find the nearest month-day in the PAST only (never future). */
  function findNearestMonthDay(target: string, dates: string[]): string {
    const targetDOY = monthDayToDOY(target);
    // Only consider dates on or before target
    const pastDates = dates.filter(d => monthDayToDOY(d) <= targetDOY);
    const pool = pastDates.length > 0 ? pastDates : dates;
    let best = pool[pool.length - 1]; // default: latest past date
    let bestDist = Infinity;
    for (const d of pool) {
      const doy = monthDayToDOY(d);
      const dist = targetDOY - doy; // always non-negative (past)
      if (dist >= 0 && dist < bestDist) { bestDist = dist; best = d; }
    }
    return best;
  }

  // Autocomplete state
  let whoOptions = $state<string[]>([]);
  let whereOptions = $state<string[]>([]);
  let showWhoDropdown = $state<Record<string, boolean>>({});
  let showWhereDropdown = $state<Record<string, boolean>>({});

  // Thumbnail cache — keyed by file_path_hash, value is base64 data URL
  let thumbnailCache = $state<Record<string, string>>({});

  // Voice
  let listening = $state(false);
  let voiceText = $state("");
  let voiceForPhoto = $state<string | null>(null);

  // ── Problem 4: Photo filter ──
  let photoFilter = $state<"all" | "people" | "places" | "screenshots">("all");

  // ── Problem 5: Sorted + clustered photo groups derived from photos ──
  let displayGroups = $derived(buildDisplayGroups(photos, photoFilter));

  /** Group photos: by year (newest first), then time-cluster within each year (30-min gap). */
  function buildDisplayGroups(all: Photo[], filter: string): { label: string; photos: Photo[] }[] {
    let filtered = all;
    if (filter === "people") {
      filtered = all.filter(p => hasWho(p) || (p.scene_tags && p.scene_tags.includes("people")));
    } else if (filter === "places") {
      filtered = all.filter(p =>
        (p.scene_tags && (p.scene_tags.includes("outdoor") || p.scene_tags.includes("urban") || p.scene_tags.includes("nature")))
        && !hasWho(p) && (!p.scene_tags || !p.scene_tags.includes("people"))
      );
    } else if (filter === "screenshots") {
      filtered = all.filter(p => /screenshot|截屏|截图/i.test(p.file_name));
    }

    // Sort by taken_at ASC (oldest first)
    const sorted = [...filtered].sort((a, b) => (a.taken_at || 0) - (b.taken_at || 0));

    // Separate by year
    const byYear = new Map<number, Photo[]>();
    for (const p of sorted) {
      if (p.taken_at) {
        const year = new Date(p.taken_at * 1000).getFullYear();
        if (!byYear.has(year)) byYear.set(year, []);
        byYear.get(year)!.push(p);
      }
    }

    // Sort years newest first
    const years = [...byYear.keys()].sort((a, b) => b - a);

    const groups: { label: string; photos: Photo[] }[] = [];
    for (const year of years) {
      const yearPhotos = byYear.get(year)!;
      let current: Photo[] = [];
      for (let i = 0; i < yearPhotos.length; i++) {
        if (i > 0 && yearPhotos[i].taken_at && yearPhotos[i - 1].taken_at) {
          const gap = yearPhotos[i].taken_at! - yearPhotos[i - 1].taken_at!;
          if (gap > 30 * 60) {
            groups.push({ label: `${year} · ${groupLabel(current)}`, photos: [...current] });
            current = [];
          }
        }
        current.push(yearPhotos[i]);
      }
      if (current.length > 0) groups.push({ label: `${year} · ${groupLabel(current)}`, photos: [...current] });
    }
    return groups;
  }

  function photoScore(p: Photo): number {
    const ann = annotations[p.file_path_hash];
    const allFilled = ann && ann.who && ann.where_place && ann.event;
    if (allFilled) return 0;                // fully annotated
    const hasPeople = p.scene_tags && p.scene_tags.includes("people");
    if (hasPeople) return 1;                // detected people
    const partial = ann && (ann.who || ann.where_place || ann.event);
    if (partial) return 2;                  // partially annotated
    return 3;                               // rest
  }

  function hasWho(p: Photo): boolean {
    const ann = annotations[p.file_path_hash];
    return !!(ann && ann.who);
  }

  /// Lightweight burst dedup: group by time gap ≤ 10s, keep largest file per group.
  function dedupBurstLight(photos: Photo[]): Photo[] {
    const sorted = [...photos].sort((a, b) => (a.taken_at || 0) - (b.taken_at || 0));
    const groups: Photo[][] = [];
    for (const p of sorted) {
      const last = groups[groups.length - 1];
      if (last && p.taken_at && last[last.length - 1].taken_at) {
        const gap = p.taken_at - last[last.length - 1].taken_at!;
        if (gap <= 10) { last.push(p); continue; }
      }
      groups.push([p]);
    }
    return groups.map(g => {
      if (g.length <= 1) return g[0];
      // Keep the largest file in the burst
      return g.reduce((best, cur) => (cur.file_size || 0) > (best.file_size || 0) ? cur : best);
    });
  }

  function dateConfidence(photo: Photo): 'high' | 'medium' | 'low' | 'very-low' {
    const source = photo.timestamp_source || 'file_modified';
    if (source === 'exif') return 'high';
    if (source === 'filename') return 'medium';
    const size = photo.file_size || 0;
    if (size >= 500_000) return 'low';
    return 'very-low';
  }

  function confidenceLabel(level: string): string {
    switch (level) {
      case 'very-low': return 'Uncertain date · small file';
      case 'low': return 'Uncertain date';
      default: return '';
    }
  }

  function groupLabel(photos: Photo[]): string {
    if (photos.length === 0) return "";
    const first = photos[0];
    const last = photos[photos.length - 1];
    const ft = first.taken_at ? new Date(first.taken_at * 1000) : null;
    const lt = last.taken_at ? new Date(last.taken_at * 1000) : null;
    const tf = (d: Date) => d.toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit" });
    const time = ft && lt ? `${tf(ft)} - ${tf(lt)}` : "";
    const count = `${photos.length} photo${photos.length > 1 ? "s" : ""}`;
    return `${time}  (${count})`;
  }

  // ── Problem 4: Delete photo ──
  async function deletePhoto(hash: string) {
    try {
      await invoke("delete_photo", { filePathHash: hash });
      photos = photos.filter(p => p.file_path_hash !== hash);
      // Clean up thumbnail cache
      const newCache = { ...thumbnailCache };
      delete newCache[hash];
      thumbnailCache = newCache;
      if (selectedPhoto === hash) selectedPhoto = null;
    } catch (e) {
      console.error("Delete failed:", e);
    }
  }

  $effect(() => {
    initDates();
  });

  async function initDates() {
    loading = true;
    produceToday(); // mark today as produced (first visit)

    // Load today's photos first — fast path, no waiting for full date list
    await loadDate(todayMD, false, false);

    // Background: fetch all available dates for the picker (non-blocking)
    try {
      availableDates = await invoke<string[]>("get_available_dates");
      if (availableDates.length > 0) {
        if (!availableDates.includes(todayMD)) {
          // Today has no photos, fallback to nearest past date
          currentDate = findNearestMonthDay(todayMD, availableDates);
          dateFallback = true;
          dateIndex = availableDates.indexOf(currentDate);
          await loadDate(currentDate, false, true);
        } else {
          dateIndex = availableDates.indexOf(todayMD);
        }
      }
    } catch (e) {
      console.error("get_available_dates failed:", e);
    }
  }

  /** isFallback=true → apply borrow logic (filter banned, mark borrowed). Manual nav → full photos. */
  async function loadDate(date: string, silent = false, isFallback = false) {
    if (!silent) loading = true;
    currentDate = date;
    dateFallback = date !== todayMD;
    try {
      let result = await invoke<Photo[]>("get_photos_by_date", { date });

      // Lightweight burst dedup: time window ≤ 10s, keep largest file
      if (result.length > 1) {
        result = dedupBurstLight(result);
      }

      if (isFallback && result.length > 0) {
        // Filter out banned (borrowed within 3 months) photos
        const available = result.filter(p => !isPhotoBanned(p.file_path_hash));
        if (available.length > 0) {
          // Mark remaining photos as borrowed
          markPhotosBorrowed(available.map(p => p.file_path_hash));
          result = available;
        }
        // If ALL photos are banned, result becomes empty → UI shows "no photos"
      }

      photos = result;

      const anns: Record<string, Annotation> = {};
      for (const p of result) {
        if (p.who || p.where_place || p.event) {
          anns[p.file_path_hash] = {
            file_path_hash: p.file_path_hash,
            who: p.who || "",
            where_place: p.where_place || "",
            event: p.event || "",
          };
        }
      }
      annotations = anns;

      // Background classify: if any photo lacks scene_tags, classify on-demand.
      if (!silent && result.some(p => !p.scene_tags || p.scene_tags.length === 0)) {
        classifyBackground(date);
      }

      // Preload thumbnails as base64 data URLs
      preloadThumbnails();
    } catch (e) {
      console.error("Failed to load photos:", e);
    } finally {
      if (!silent) loading = false;
    }
  }

  async function preloadThumbnails() {
    const toLoad = photos.filter(p => p.thumbnail_path && !thumbnailCache[p.file_path_hash]);
    if (toLoad.length === 0) return;
    const results = await Promise.all(
      toLoad.map(async (p) => {
        try {
          const dataUrl = await invoke<string | null>("read_thumbnail_base64", { hash: p.file_path_hash });
          return { hash: p.file_path_hash, dataUrl };
        } catch {
          return { hash: p.file_path_hash, dataUrl: null };
        }
      })
    );
    for (const { hash, dataUrl } of results) {
      if (dataUrl) {
        thumbnailCache = { ...thumbnailCache, [hash]: dataUrl };
      }
    }
  }

  async function classifyBackground(date: string) {
    try {
      const count = await invoke<number>("classify_date", { date });
      if (count > 0 && currentDate === date) {
        // Silent reload to pick up scene_tags + thumbnails
        await loadDate(date, true);
      }
    } catch (e) {
      console.error("Background classify failed:", e);
    }
  }

  /// Pre-classify the next 7 month-days in background.
  async function preclassifyNearby() {
    const [m, d] = todayMD.split('-').map(Number);
    const base = new Date(2000, m - 1, d);
    for (let i = 1; i <= 7; i++) {
      const next = new Date(base);
      next.setDate(base.getDate() + i);
      const mm = String(next.getMonth() + 1).padStart(2, '0');
      const dd = String(next.getDate()).padStart(2, '0');
      const nextMD = `${mm}-${dd}`;
      try {
        await invoke<number>("classify_date", { date: nextMD });
      } catch { /* best-effort */ }
    }
  }

  function goToDate(delta: number) {
    // Navigate by month-day using LOCAL time (avoid toISOString UTC offset bug)
    const [m, d] = currentDate.split('-').map(Number);
    const base = new Date(2000, m - 1, d);
    base.setDate(base.getDate() + delta);
    const mm = String(base.getMonth() + 1).padStart(2, '0');
    const dd = String(base.getDate()).padStart(2, '0');
    const newDate = `${mm}-${dd}`;
    currentDate = newDate;
    const idx = availableDates.indexOf(newDate);
    dateIndex = idx >= 0 ? idx : -1;
    loadDate(newDate);
  }

  const EMPTY_ANN: Annotation = { file_path_hash: "", who: "", where_place: "", event: "" };

  /// Pure read — safe to call in template expressions.
  /// Returns an empty annotation if none exists, without mutating any $state.
  function readAnnotation(hash: string): Annotation {
    return annotations[hash] ?? { file_path_hash: hash, who: "", where_place: "", event: "" };
  }

  /// Mutable access — creates the annotation entry if missing.
  /// Only call from event handlers, never from template expressions.
  function mutAnnotation(hash: string): Annotation {
    if (!annotations[hash]) {
      annotations = { ...annotations, [hash]: { file_path_hash: hash, who: "", where_place: "", event: "" } };
    }
    return annotations[hash];
  }

  async function saveAnnotation(hash: string) {
    const ann = readAnnotation(hash);
    try {
      await invoke("save_annotation", {
        input: {
          file_path_hash: hash,
          calendar_date: currentDate,
          who: ann.who || null,
          where_place: ann.where_place || null,
          event: ann.event || null,
        },
      });
      // Mark as saved locally (but not yet synced to Worker)
    } catch (e) {
      console.error("Save annotation failed:", e);
    }
  }

  // ── Save (per date): flush all local + sync to Worker(pending) + generate story (best-effort) + store locally ──
  async function handleSaveDate() {
    if (isSaved || syncing) return;
    syncing = true;
    saveMessage = "Saving...";
    try {
      // Build full YYYY-MM-DD from the annotation date (currentDate = MM-DD)
      const fullDate = `${today.slice(0, 4)}-${currentDate}`;

      // Flush all in-memory annotations to local DB
      for (const hash of Object.keys(annotations)) {
        const ann = annotations[hash];
        if (ann.who || ann.where_place || ann.event) {
          await invoke("save_annotation", {
            input: {
              file_path_hash: hash,
              calendar_date: currentDate,
              who: ann.who || null,
              where_place: ann.where_place || null,
              event: ann.event || null,
            },
          });
        }
      }
      // Sync to Worker as pending — must succeed
      await invoke("sync_date", { date: currentDate, fullDate });

      // Mark saved immediately: sync succeeded, save is valid
      savedDates = { ...savedDates, [currentDate]: true };
      persistSavedDates();
      saveMessage = `Saved! ${currentDate}.`;

      // Generate story on Worker (best-effort, failure doesn't undo save)
      try {
        const storyResult = await generateStory(token, fullDate, true);
        await invoke("save_story", {
          date: fullDate,
          title: storyResult.story.title,
          content: storyResult.story.content,
        });
        saveMessage = `Saved! Story ready for ${currentDate}.`;
      } catch {
        // Fallback: Cron may have already generated this story in D1 — pull it now
        try {
          const existing = await getStory(token, fullDate);
          if (existing?.story?.title && existing?.story?.content) {
            await invoke("save_story", {
              date: fullDate,
              title: existing.story.title,
              content: existing.story.content,
            });
            saveMessage = `Saved! Story ready for ${currentDate}.`;
          }
        } catch (_) { /* fallback silently, story stays pending for Cron */ }
      }

      // Wait 1.5s for user to see feedback, then auto-navigate
      await new Promise(r => setTimeout(r, 1500));
      saveMessage = "";

      // Auto-navigate to next unsaved date within 7 days
      await navigateToNextUnsaved();
    } catch (e: any) {
      console.error("Save date failed:", e);
      saveMessage = `Failed: ${e.message || e}`;
    } finally {
      syncing = false;
    }
  }

  async function navigateToNextUnsaved() {
    // Get all dates within the 7-day window that have photos
    const today = new Date();
    const windowEnd = new Date(today);
    windowEnd.setDate(windowEnd.getDate() + 6);

    const candidates: string[] = [];
    for (const d of (availableDates.length > 0 ? availableDates : [todayMD])) {
      // d is MM-DD format
      const [m, dStr] = d.split("-").map(Number);
      const dateInWindow = new Date(today.getFullYear(), m - 1, dStr);
      if (dateInWindow >= today && dateInWindow <= windowEnd && d !== currentDate) {
        if (!savedDates[d]) {
          candidates.push(d);
        }
      }
    }
    if (candidates.length > 0) {
      currentDate = candidates[0];
      dateIndex = availableDates.indexOf(currentDate);
      dateFallback = currentDate !== todayMD;
      await loadDate(currentDate, false, false);
    }
  }

  async function fetchAutocomplete(field: "who" | "where", prefix: string) {
    try {
      const cmd = field === "who" ? "autocomplete_who" : "autocomplete_where";
      const results = await invoke<string[]>(cmd, { prefix });
      if (field === "who") whoOptions = results;
      else whereOptions = results;
    } catch (e) {
      console.error(`Autocomplete ${field} failed:`, e);
    }
  }

  function onWhoFocus(hash: string) {
    showWhoDropdown = { ...showWhoDropdown, [hash]: true };
    fetchAutocomplete("who", readAnnotation(hash).who);
  }
  function onWhoBlur(hash: string) {
    setTimeout(() => {
      showWhoDropdown = { ...showWhoDropdown, [hash]: false };
    }, 150);
  }
  function onWhereFocus(hash: string) {
    showWhereDropdown = { ...showWhereDropdown, [hash]: true };
    fetchAutocomplete("where", readAnnotation(hash).where_place);
  }
  function onWhereBlur(hash: string) {
    setTimeout(() => {
      showWhereDropdown = { ...showWhereDropdown, [hash]: false };
    }, 150);
  }

  function selectAutocomplete(hash: string, field: string, value: string) {
    const ann = mutAnnotation(hash);
    if (field === "who") ann.who = value;
    else ann.where_place = value;
    annotations = { ...annotations };
    showWhoDropdown = { ...showWhoDropdown, [hash]: false };
    showWhereDropdown = { ...showWhereDropdown, [hash]: false };
    saveAnnotation(hash);
  }

  function onFieldBlur(hash: string) {
    saveAnnotation(hash);
  }

  function formatTime(ts: number | null): string {
    if (!ts) return "";
    const d = new Date(ts * 1000);
    return d.toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit" });
  }

  // ── Voice Input ──

  function startVoice(hash: string) {
    const SpeechRecognition = (window as any).SpeechRecognition || (window as any).webkitSpeechRecognition;
    if (!SpeechRecognition) {
      voiceText = "Speech recognition not available in this environment.";
      return;
    }
    voiceForPhoto = hash;
    voiceText = "";
    listening = true;

    const rec = new SpeechRecognition();
    rec.lang = "en-US";
    rec.interimResults = false;
    rec.maxAlternatives = 1;

    rec.onresult = async (event: any) => {
      const text = event.results[0][0].transcript;
      voiceText = text;
      listening = false;

      // Call AI to split into who/where/event
      try {
        const parsed = await parseAnnotation(token, text);
        const ann = mutAnnotation(hash);
        if (parsed.who) ann.who = parsed.who;
        if (parsed.where) ann.where_place = parsed.where;
        if (parsed.event) ann.event = parsed.event;
        annotations = { ...annotations };
        saveAnnotation(hash);
        voiceText = `Parsed: ${parsed.who || "?"} | ${parsed.where || "?"} | ${parsed.event || "?"}`;
      } catch {
        voiceText = `Heard: "${text}" (AI parsing failed, fill manually)`;
      }
    };

    rec.onerror = (event: any) => {
      voiceText = `Voice error: ${event.error}`;
      listening = false;
    };

    rec.start();
  }

  // Shared scan save + refresh logic
  async function finishScan(result: any[]) {
    console.log("[finishScan] received", result.length, "photos");
    if (result.length === 0) {
      scanStatus = "No photos found.";
      return;
    }
    scanStatus = `Found ${result.length} photos. Saving...`;
    const saved = await invoke<number>("save_scanned_photos", { photos: result });
    console.log("[finishScan] saved", saved, "photos");
    scanStatus = `Saved ${saved} photos. Refreshing...`;

    availableDates = await invoke<string[]>("get_available_dates");
    if (availableDates.includes(todayMD)) {
      currentDate = todayMD;
      dateFallback = false;
    } else if (availableDates.length > 0) {
      currentDate = findNearestMonthDay(todayMD, availableDates);
      dateFallback = true;
    }
    dateIndex = availableDates.indexOf(currentDate);
    await loadDate(currentDate, false, dateFallback);
    scanStatus = "";
  }

  // ── P3a: Manual multi-folder scan ──
  async function scanFolders() {
    scanning = true;
    let unlistenProgress: (() => void) | null = null;
    let unlistenComplete: (() => void) | null = null;
    try {
      const selected = await open({ directory: true, multiple: true, title: "Select photo folder(s)" });
      if (!selected) {
        scanStatus = "";
        scanning = false;
        return;
      }

      const folders: string[] = Array.isArray(selected) ? selected : [selected as string];
      scanStatus = `Scanning ${folders.length} folder(s)...`;

      unlistenProgress = await listen("scan_progress", (event: any) => {
        const p = event.payload;
        const phaseLabel = p.phase === 1 ? "Discovering" : "EXIF";
        let line = `${phaseLabel}: ${p.photos_found} photos (${p.parsed} parsed)`;
        if (p.folder_index && p.total_folders) {
          line = `[${p.folder_index}/${p.total_folders}] ${p.current_path}\n${line}`;
        }
        scanStatus = line;
      });

      unlistenComplete = await listen("scan_complete", (event: any) => {
        scanStatus = `Scan done: ${event.payload.total_photos} photos found. Saving...`;
      });

      // Scan each folder sequentially, merge results
      let allResults: any[] = [];
      for (let i = 0; i < folders.length; i++) {
        scanStatus = `Folder ${i + 1}/${folders.length}: ${folders[i]}`;
        const result = await invoke<any[]>("scan_directory", { dirPath: folders[i] });
        allResults = allResults.concat(result);
      }

      await finishScan(allResults);
    } catch (e: any) {
      console.error("Scan failed:", e);
      scanStatus = `Scan error: ${e.message || e}`;
    } finally {
      if (unlistenProgress) unlistenProgress();
      if (unlistenComplete) unlistenComplete();
      scanning = false;
    }
  }

  // ── P3b: Full auto-scan all drives ──
  async function scanAllDrives() {
    scanning = true;
    scanStatus = "Enumerating drives...";
    let unlistenProgress: (() => void) | null = null;
    let unlistenComplete: (() => void) | null = null;
    try {
      unlistenProgress = await listen("scan_progress", (event: any) => {
        const p = event.payload;
        const phaseLabel = p.phase === 1 ? "Discovering" : "EXIF";
        let line = `${phaseLabel}: ${p.photos_found} photos (${p.parsed} parsed)`;
        if (p.folder_index && p.total_folders) {
          line = `[${p.folder_index}/${p.total_folders}] ${p.current_path}\n${line}`;
          if (p.total_photos_so_far > 0) {
            line += ` | Total: ${p.total_photos_so_far}`;
          }
        }
        scanStatus = line;
      });

      unlistenComplete = await listen("scan_complete", (event: any) => {
        scanStatus = `All drives scanned: ${event.payload.total_photos} photos found. Saving...`;
      });

      const result = await invoke<any[]>("start_auto_scan");
      await finishScan(result);
    } catch (e: any) {
      console.error("Auto-scan failed:", e);
      scanStatus = `Scan error: ${e.message || e}`;
    } finally {
      if (unlistenProgress) unlistenProgress();
      if (unlistenComplete) unlistenComplete();
      scanning = false;
    }
  }

  function getPhotoUrl(photo: Photo): string {
    // Use cached base64 thumbnail if available (bypasses Tauri asset protocol)
    if (thumbnailCache[photo.file_path_hash]) {
      return thumbnailCache[photo.file_path_hash];
    }
    // Gray placeholder while thumbnail loads
    return "data:image/svg+xml," + encodeURIComponent(
      `<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80" fill="#e5e7eb"><rect width="120" height="80"/></svg>`
    );
  }
</script>

<div class="annotate-container">
  <header>
    <div class="header-left">
      <h1>That Day</h1>
      <div class="date-nav">
        <button class="arrow-btn" onclick={() => goToDate(-1)}
                disabled={producedMDs.length > 0 && currentDate <= producedMDs[0]}>&lt;</button>
        <div class="date-picker-wrap">
          <button class="date-display" onclick={() => datePickerOpen = !datePickerOpen}>
            <span class="date-main">{formatMonthDay(currentDate)}</span>
            <span class="date-sub">{yearRange}</span>
          </button>
          {#if datePickerOpen}
            <!-- svelte-ignore a11y_click_events_have_key_events -->
            <div class="date-picker-panel" role="dialog">
              <div class="picker-header">
                <span>Your days ({producedMDs.length})</span>
                <button class="picker-close" onclick={() => datePickerOpen = false}>×</button>
              </div>
              <div class="picker-list">
                {#each pickerDates as d}
                  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
                  <div
                    class="picker-item"
                    class:active={d === currentDate}
                    class:today={d === todayMD}
                    onclick={() => { loadDate(d); datePickerOpen = false; }}
                    onkeydown={(e) => e.key === 'Enter' && (loadDate(d), datePickerOpen = false)}
                    role="button"
                    tabindex="0"
                  >
                    <span class="dot" class:has-photos={true}>●</span>
                    <span>{formatMonthDay(d, "short")}</span>
                    {#if d === todayMD}<span class="today-badge">today</span>{/if}
                  </div>
                {/each}
              </div>
            </div>
          {/if}
        </div>
        <button class="arrow-btn" onclick={() => goToDate(1)}
                disabled={currentDate >= maxFutureMD}>&gt;</button>
      </div>
    </div>
    <nav>
      <div class="scan-dropdown">
        <button onclick={() => scanDropdownOpen = !scanDropdownOpen} disabled={scanning}>
          {scanning ? "Scanning..." : "Scan ▾"}
        </button>
        {#if scanDropdownOpen && !scanning}
          <div class="dropdown-menu">
            <button onclick={() => { scanDropdownOpen = false; scanAllDrives(); }}>Scan All Drives</button>
            <button onclick={() => { scanDropdownOpen = false; scanFolders(); }}>Choose Folders...</button>
          </div>
        {/if}
      </div>
      <button onclick={() => { preclassifyNearby().catch(() => {}); onStory(); }}>Stories</button>
      <button onclick={onSettings}>Settings</button>
    </nav>
  </header>

  {#if scanStatus}
    <div class="scan-banner">{scanStatus}</div>
  {/if}
  {#if dateFallback && !loading}
    <div class="fallback-banner">
      No photos for today ({formatMonthDay(todayMD, "short")}).
      Showing nearest: {formatMonthDay(currentDate, "short")}.
    </div>
  {/if}
  {#if saveMessage}
    <div class="save-banner">
      {saveMessage}
    </div>
  {/if}

  <main>
    {#if loading}
      <div class="empty"><p>Loading photos...</p></div>
    {:else if photos.length === 0}
      <div class="empty">
        <p>No photos for {formatMonthDay(currentDate, "short")}.</p>
        <p class="hint">Scan a folder or use the arrows to browse other dates.</p>
      </div>
    {:else}
      <!-- ── Filter tabs + Save ── -->
      <div class="filter-bar">
        <div class="filter-tabs">
          <button class="filter-tab" class:active={photoFilter === "all"} onclick={() => photoFilter = "all"}>All</button>
          <button class="filter-tab" class:active={photoFilter === "people"} onclick={() => photoFilter = "people"}>👤 People</button>
          <button class="filter-tab" class:active={photoFilter === "places"} onclick={() => photoFilter = "places"}>🏞 Places</button>
          <button class="filter-tab" class:active={photoFilter === "screenshots"} onclick={() => photoFilter = "screenshots"}>📸 Screenshots</button>
        </div>
        <div class="save-area">
          {#if isSaved}
            <span class="saved-badge">✓ Saved</span>
          {:else if photos.length > 0}
            <button class="save-btn" onclick={handleSaveDate} disabled={syncing}>
              {syncing ? "Saving..." : "💾 Save"}
            </button>
          {/if}
        </div>
      </div>
      {#if !isSaved && photos.length > 0}
        <div class="save-hint">Annotate your photos, then click Save.</div>
      {/if}

      <!-- ── Problem 5: Clustered photo list ── -->
      {#each displayGroups as group (group.label)}
        <div class="cluster-group">
          <div class="cluster-header">{group.label}</div>
          <div class="photo-list">
            {#each group.photos as photo (photo.file_path_hash)}
              <div
                class="photo-card"
                class:selected={selectedPhoto === photo.file_path_hash}
                class:annotated={hasWho(photo) && annotations[photo.file_path_hash]?.where_place && annotations[photo.file_path_hash]?.event}
                onclick={() => selectedPhoto = photo.file_path_hash}
                onkeydown={(e) => e.key === 'Enter' && (selectedPhoto = photo.file_path_hash)}
                role="button"
                tabindex="0"
              >
                <div class="photo-thumb">
                  <img
                    src={getPhotoUrl(photo)}
                    alt={photo.file_name}
                    loading="lazy"
                  />
                  <span class="photo-time">{formatTime(photo.taken_at)}</span>
                  {#if dateConfidence(photo) === 'low' || dateConfidence(photo) === 'very-low'}
                    <span class="conf-badge" title={confidenceLabel(dateConfidence(photo))}>ⓘ</span>
                  {/if}
                  <!-- Problem 4: Delete button -->
                  <button
                    class="delete-btn"
                    title="Remove this photo"
                    onclick={(e) => { e.stopPropagation(); deletePhoto(photo.file_path_hash); }}
                  >×</button>
                </div>
                <div class="photo-fields">
                  <!-- Who field -->
                  <div class="field-row">
                    <label for="who-{photo.file_path_hash}">Who</label>
                    <div class="autocomplete-wrap">
                      <input
                        id="who-{photo.file_path_hash}"
                        type="text"
                        placeholder="Who was with you?"
                        value={readAnnotation(photo.file_path_hash).who}
                        oninput={(e) => {
                          const ann = mutAnnotation(photo.file_path_hash);
                          ann.who = (e.target as HTMLInputElement).value;
                          annotations = { ...annotations };
                          fetchAutocomplete("who", ann.who);
                        }}
                        onfocus={() => onWhoFocus(photo.file_path_hash)}
                        onblur={() => { onWhoBlur(photo.file_path_hash); onFieldBlur(photo.file_path_hash); }}
                      />
                      {#if showWhoDropdown[photo.file_path_hash] && whoOptions.length > 0}
                        <ul class="dropdown">
                          {#each whoOptions as opt}
                            <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
                            <li
                              onclick={() => selectAutocomplete(photo.file_path_hash, "who", opt)}
                              onkeydown={(e) => e.key === 'Enter' && selectAutocomplete(photo.file_path_hash, "who", opt)}
                              tabindex="0"
                            >{opt}</li>
                          {/each}
                        </ul>
                      {/if}
                    </div>
                  </div>
                  <!-- Where field -->
                  <div class="field-row">
                    <label for="where-{photo.file_path_hash}">Where</label>
                    <div class="autocomplete-wrap">
                      <input
                        id="where-{photo.file_path_hash}"
                        type="text"
                        placeholder="Where was this taken?"
                        value={readAnnotation(photo.file_path_hash).where_place}
                        oninput={(e) => {
                          const ann = mutAnnotation(photo.file_path_hash);
                          ann.where_place = (e.target as HTMLInputElement).value;
                          annotations = { ...annotations };
                          fetchAutocomplete("where", ann.where_place);
                        }}
                        onfocus={() => onWhereFocus(photo.file_path_hash)}
                        onblur={() => { onWhereBlur(photo.file_path_hash); onFieldBlur(photo.file_path_hash); }}
                      />
                      {#if showWhereDropdown[photo.file_path_hash] && whereOptions.length > 0}
                        <ul class="dropdown">
                          {#each whereOptions as opt}
                            <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
                            <li
                              onclick={() => selectAutocomplete(photo.file_path_hash, "where", opt)}
                              onkeydown={(e) => e.key === 'Enter' && selectAutocomplete(photo.file_path_hash, "where", opt)}
                              tabindex="0"
                            >{opt}</li>
                          {/each}
                        </ul>
                      {/if}
                    </div>
                  </div>
                  <!-- Event field -->
                  <div class="field-row">
                    <label for="event-{photo.file_path_hash}">Event</label>
                    <input
                      id="event-{photo.file_path_hash}"
                      type="text"
                      placeholder="What was happening?"
                      value={readAnnotation(photo.file_path_hash).event}
                      oninput={(e) => {
                        const ann = mutAnnotation(photo.file_path_hash);
                        ann.event = (e.target as HTMLInputElement).value;
                        annotations = { ...annotations };
                      }}
                      onblur={() => onFieldBlur(photo.file_path_hash)}
                    />
                  </div>
                  <!-- Voice button -->
                  <button
                    class="voice-btn"
                    class:listening={listening && voiceForPhoto === photo.file_path_hash}
                    onclick={(e) => { e.stopPropagation(); startVoice(photo.file_path_hash); }}
                    title="Hold to speak"
                  >
                    🎤
                  </button>
                </div>
              </div>
            {/each}
          </div>
        </div>
      {/each}
    {/if}
  </main>
</div>

<style>
  .annotate-container {
    display: flex;
    flex-direction: column;
    min-height: 100vh;
    font-family: system-ui, -apple-system, sans-serif;
    background: #f9fafb;
  }

  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 1rem 2rem;
    border-bottom: 1px solid #e5e7eb;
    background: white;
    position: sticky;
    top: 0;
    z-index: 100;
  }

  .header-left {
    display: flex;
    align-items: baseline;
    gap: 1rem;
  }

  h1 {
    font-size: 1.25rem;
    color: #1f2937;
    margin: 0;
  }

  .date {
    font-size: 0.875rem;
    color: #6b7280;
  }

  nav {
    display: flex;
    gap: 0.5rem;
    align-items: center;
  }

  nav button {
    padding: 0.5rem 1rem;
    border: 1px solid #d1d5db;
    border-radius: 6px;
    background: white;
    cursor: pointer;
    font-size: 0.875rem;
    color: #374151;
    transition: background 0.15s;
  }

  nav button:hover {
    background: #f3f4f6;
  }

  .scan-dropdown {
    position: relative;
  }

  .dropdown-menu {
    position: absolute;
    top: 100%;
    left: 0;
    margin-top: 4px;
    background: white;
    border: 1px solid #e5e7eb;
    border-radius: 8px;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.1);
    z-index: 200;
    min-width: 180px;
    overflow: hidden;
  }

  .dropdown-menu button {
    display: block;
    width: 100%;
    padding: 0.6rem 1rem;
    border: none;
    border-radius: 0;
    background: white;
    cursor: pointer;
    font-size: 0.875rem;
    color: #374151;
    text-align: left;
    transition: background 0.1s;
  }

  .dropdown-menu button:hover {
    background: #eff6ff;
    color: #1e40af;
  }

  .dropdown-menu button + button {
    border-top: 1px solid #f3f4f6;
  }

  .date-nav {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .arrow-btn {
    width: 28px;
    height: 28px;
    border: 1px solid #d1d5db;
    border-radius: 6px;
    background: white;
    cursor: pointer;
    font-size: 0.875rem;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    color: #374151;
  }
  .arrow-btn:hover:not(:disabled) {
    background: #f3f4f6;
  }
  .arrow-btn:disabled {
    opacity: 0.3;
    cursor: not-allowed;
  }
  .date-picker-wrap {
    position: relative;
  }
  .date-display {
    font-size: 0.875rem;
    color: #6b7280;
    background: none;
    border: none;
    border-bottom: 1px dashed #c4c4c4;
    padding: 0 0 1px;
    cursor: pointer;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 1px;
  }
  .date-display:hover {
    color: #374151;
    border-color: #6b7280;
  }
  .date-main {
    font-weight: 600;
    color: #1f2937;
  }
  .date-sub {
    font-size: 0.65rem;
    color: #9ca3af;
    font-weight: 400;
  }
  .date-picker-panel {
    position: absolute;
    top: calc(100% + 6px);
    left: 50%;
    transform: translateX(-50%);
    background: white;
    border: 1px solid #e5e7eb;
    border-radius: 10px;
    box-shadow: 0 8px 30px rgba(0,0,0,0.12);
    z-index: 300;
    min-width: 240px;
    max-height: 320px;
    display: flex;
    flex-direction: column;
  }
  .picker-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 0.6rem 0.75rem;
    font-size: 0.8rem;
    font-weight: 600;
    color: #374151;
    border-bottom: 1px solid #f3f4f6;
  }
  .picker-close {
    width: 22px;
    height: 22px;
    border: none;
    background: #f3f4f6;
    border-radius: 50%;
    cursor: pointer;
    font-size: 0.85rem;
    line-height: 1;
    color: #6b7280;
    padding: 0;
  }
  .picker-close:hover { background: #e5e7eb; }
  .picker-list {
    overflow-y: auto;
    flex: 1;
  }
  .picker-item {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.45rem 0.75rem;
    cursor: pointer;
    font-size: 0.85rem;
    color: #374151;
    transition: background 0.1s;
  }
  .picker-item:hover { background: #f3f4f6; }
  .picker-item.active { background: #eff6ff; color: #1d4ed8; }
  .picker-item.today { font-weight: 600; }
  .today-badge {
    font-size: 0.6rem;
    background: #dbeafe;
    color: #1d4ed8;
    padding: 1px 5px;
    border-radius: 3px;
    margin-left: auto;
  }
  .dot {
    font-size: 0.5rem;
    flex-shrink: 0;
  }
  .dot.has-photos { color: #10b981; }
  .scan-banner {
    padding: 0.5rem 2rem;
    background: #fef3c7;
    color: #92400e;
    font-size: 0.875rem;
    text-align: center;
    white-space: pre-line;
    line-height: 1.5;
  }
  .fallback-banner {
    padding: 0.5rem 2rem;
    background: #ede9fe;
    color: #5b21b6;
    font-size: 0.8rem;
    text-align: center;
  }
  .save-banner {
    padding: 0.75rem 2rem;
    background: #dbeafe;
    color: #1e40af;
    font-size: 0.875rem;
    text-align: center;
  }

  /* ── Filter bar ── */
  .filter-bar {
    display: flex;
    gap: 0.5rem;
    padding: 0 2rem 1rem;
    max-width: 720px;
    margin: 0 auto;
  }
  .filter-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 2rem;
    border-bottom: 1px solid #f3f4f6;
  }
  .filter-tabs {
    display: flex;
    gap: 0.5rem;
  }
  .filter-tab {
    padding: 0.35rem 0.85rem;
    border: 1px solid #d1d5db;
    border-radius: 20px;
    background: white;
    font-size: 0.8rem;
    color: #6b7280;
    cursor: pointer;
    transition: all 0.15s;
  }
  .filter-tab:hover { background: #f3f4f6; color: #374151; }
  .filter-tab.active {
    background: #1d4ed8;
    border-color: #1d4ed8;
    color: white;
  }

  /* ── Cluster groups ── */
  .cluster-group {
    max-width: 720px;
    margin: 0 auto 1.5rem;
  }
  .cluster-header {
    font-size: 0.8rem;
    font-weight: 600;
    color: #6b7280;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    padding: 0.5rem 0.25rem 0.25rem;
    border-bottom: 2px solid #f3f4f6;
    margin-bottom: 0.5rem;
  }

  /* ── Delete button ── */
  .delete-btn {
    position: absolute;
    top: 4px;
    right: 4px;
    width: 22px;
    height: 22px;
    border: none;
    border-radius: 50%;
    background: rgba(0,0,0,0.35);
    color: white;
    font-size: 0.85rem;
    line-height: 1;
    cursor: pointer;
    opacity: 0;
    transition: opacity 0.15s, background 0.15s;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    z-index: 5;
  }
  .photo-thumb:hover .delete-btn { opacity: 1; }
  .delete-btn:hover { background: rgba(239,68,68,0.85); }

  /* ── Annotated card highlight ── */
  .photo-card.annotated {
    border-color: #93c5fd;
    border-left: 3px solid #3b82f6;
  }

  main {
    flex: 1;
    padding: 1.5rem 2rem;
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 50vh;
    color: #6b7280;
  }

  .empty p {
    margin: 0.25rem 0;
  }

  .hint {
    font-size: 0.875rem;
    color: #9ca3af;
  }

  .photo-list {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  .photo-card {
    display: flex;
    gap: 1rem;
    background: white;
    border: 2px solid #e5e7eb;
    border-radius: 10px;
    padding: 0.75rem;
    cursor: pointer;
    transition: border-color 0.15s, box-shadow 0.15s;
  }

  .photo-card:hover {
    border-color: #93c5fd;
  }

  .photo-card.selected {
    border-color: #3b82f6;
    box-shadow: 0 0 0 3px rgba(59, 130, 246, 0.15);
  }

  .photo-thumb {
    position: relative;
    width: 120px;
    height: 80px;
    border-radius: 6px;
    overflow: hidden;
    flex-shrink: 0;
    background: #f3f4f6;
  }

  .photo-thumb img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .photo-time {
    position: absolute;
    bottom: 4px;
    right: 4px;
    background: rgba(0, 0, 0, 0.6);
    color: white;
    font-size: 0.7rem;
    padding: 2px 6px;
    border-radius: 4px;
  }

  .conf-badge {
    position: absolute;
    bottom: 4px;
    left: 4px;
    background: rgba(245, 158, 11, 0.85);
    color: white;
    font-size: 0.65rem;
    font-weight: 700;
    padding: 1px 5px;
    border-radius: 3px;
    line-height: 1;
    cursor: help;
  }

  .photo-fields {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    position: relative;
  }

  .field-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .field-row label {
    width: 42px;
    font-size: 0.75rem;
    font-weight: 600;
    color: #6b7280;
    text-transform: uppercase;
    text-align: right;
  }

  .field-row input {
    flex: 1;
    padding: 0.35rem 0.5rem;
    border: 1px solid #e5e7eb;
    border-radius: 4px;
    font-size: 0.875rem;
    color: #1f2937;
    outline: none;
    transition: border-color 0.15s;
  }

  .field-row input:focus {
    border-color: #3b82f6;
  }

  .autocomplete-wrap {
    flex: 1;
    position: relative;
  }

  .autocomplete-wrap input {
    width: 100%;
    box-sizing: border-box;
  }

  .dropdown {
    position: absolute;
    top: 100%;
    left: 0;
    right: 0;
    background: white;
    border: 1px solid #e5e7eb;
    border-radius: 4px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.1);
    z-index: 50;
    list-style: none;
    margin: 2px 0 0;
    padding: 0.25rem 0;
    max-height: 160px;
    overflow-y: auto;
  }

  .dropdown li {
    padding: 0.4rem 0.75rem;
    cursor: pointer;
    font-size: 0.875rem;
    color: #374151;
  }

  .dropdown li:hover {
    background: #eff6ff;
    color: #1e40af;
  }

  .save-area {
    margin-left: auto;
    display: flex;
    align-items: center;
  }
  .save-btn {
    background: #2563eb;
    color: white;
    border: none;
    padding: 0.35rem 0.75rem;
    border-radius: 6px;
    font-size: 0.8rem;
    font-weight: 500;
    cursor: pointer;
    font-family: inherit;
    transition: background 0.15s;
  }
  .save-btn:hover {
    background: #1d4ed8;
  }
  .save-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .saved-badge {
    font-size: 0.8rem;
    color: #16a34a;
    font-weight: 500;
  }
  .save-hint {
    text-align: center;
    padding: 0.5rem;
    font-size: 0.8rem;
    color: #6b7280;
    background: #f0fdf4;
    border-bottom: 1px solid #bbf7d0;
  }

  .voice-btn {
    position: absolute;
    bottom: 0.25rem;
    right: 0.25rem;
    width: 32px;
    height: 32px;
    border: 1px solid #d1d5db;
    border-radius: 50%;
    background: white;
    cursor: pointer;
    font-size: 1rem;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    transition: background 0.15s, border-color 0.15s;
  }

  .voice-btn:hover {
    background: #fef2f2;
    border-color: #ef4444;
  }

  .voice-btn.listening {
    background: #ef4444;
    border-color: #ef4444;
    animation: pulse 1s infinite;
  }

  .scene-tags {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    padding-left: 50px;
  }
  .tag {
    font-size: 0.65rem;
    padding: 1px 6px;
    background: #e0e7ff;
    color: #3730a3;
    border-radius: 3px;
    text-transform: capitalize;
  }

  @keyframes pulse {
    0%, 100% { box-shadow: 0 0 0 0 rgba(239, 68, 68, 0.4); }
    50% { box-shadow: 0 0 0 8px rgba(239, 68, 68, 0); }
  }
</style>
