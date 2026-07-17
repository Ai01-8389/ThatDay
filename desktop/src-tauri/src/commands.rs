use crate::db::{get_db, AppState};
use crate::scanner;
use crate::scanner::progress::ScanProgress;
use crate::types::{AnnotationInput, PhotoMeta, PhotoWithAnnotation};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};

// ── ThatDay cross-year window ──
/// Earliest year included in the ThatDay view (testing baseline).
const YEAR_WINDOW_START: i32 = 2015;

/// Compute the Unix timestamp bounds for the cross-year photo window.
/// Returns (start_of_start_year, end_of_current_year).
fn year_window_bounds() -> (i64, i64) {
    use chrono::Datelike;
    let now = chrono::Utc::now();
    let current_year = now.year();
    let start = chrono::NaiveDate::from_ymd_opt(YEAR_WINDOW_START, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0);
    let end = chrono::NaiveDate::from_ymd_opt(current_year, 12, 31)
        .and_then(|d| d.and_hms_opt(23, 59, 59))
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(i64::MAX);
    (start, end)
}

/// Scan a directory for photos, extract EXIF.
/// Only Phase 1 (discovery) + Phase 2 (EXIF).
/// Classification and thumbnails are deferred to `classify_date`.
#[tauri::command]
pub async fn scan_directory(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    dir_path: String,
) -> Result<Vec<PhotoMeta>, String> {
    let path = PathBuf::from(&dir_path);
    if !path.is_dir() {
        return Err("Directory does not exist".into());
    }

    let progress = Arc::new(ScanProgress::with_app(app.clone()));

    // Phase 1: discover photos
    progress.set_phase(1, 0);
    let discovered = scanner::discovery::discover_photos(&path, &progress)
        .map_err(|e| format!("Discovery error: {}", e))?;

    if discovered.is_empty() {
        return Ok(vec![]);
    }

    progress.set_phase(2, discovered.len());

    // Phase 2: parallel EXIF extraction with progress
    let discovered_len = discovered.len();
    let progress_clone = progress.clone();
    let enriched = scanner::exif::enrich_parallel(discovered, move |parsed| {
        progress_clone.set_parsed(parsed);
    });



    // Filter
    let filter = scanner::filter::PhotoFilter::default();
    let mut result: Vec<PhotoMeta> = enriched
        .into_iter()
        .filter(|p| filter.passes(p))
        .collect();

    // Deduplicate: content hash (Layer 1) + burst shots (Layer 2)
    result = scanner::dedup::dedup(result);

    let count = result.len() as u64;
    progress.mark_done();

    println!("[scan_directory] {} discovered={} after_filter={}", path.display(), discovered_len, count);

    app.emit("scan_complete", serde_json::json!({
        "total_photos": count,
        "status": "done"
    })).map_err(|e| e.to_string())?;

    Ok(result)
}

/// Start auto-scan on default photo locations (non-C drives first, then user folders).
/// Emits `scan_progress` (with folder_index/total_folders) and `scan_complete` events.
/// Only Phase 1 (discovery) + Phase 2 (EXIF) — classification is deferred per route B.
#[tauri::command]
pub async fn start_auto_scan(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
) -> Result<Vec<PhotoMeta>, String> {
    let paths = get_default_scan_paths();
    if paths.is_empty() {
        return Err("No scan paths found".into());
    }

    let total = paths.len();
    let progress = Arc::new(ScanProgress::with_app(app.clone()));
    let progress_inner = progress.clone();
    let app_clone = app.clone();

    // Run scan on blocking thread so Tauri events flow
    let all_photos = tokio::task::spawn_blocking(move || -> Result<Vec<PhotoMeta>, String> {
        let mut all: Vec<PhotoMeta> = Vec::new();

        for (i, path_str) in paths.iter().enumerate() {
            // Update folder-level progress info
            progress_inner.set_folder_info(i + 1, total);
            progress_inner.set_current_path(path_str);
            progress_inner.reset_folder_counters();

            let path = PathBuf::from(path_str);
            if !path.is_dir() {
                continue;
            }

            // Phase 1: discover
            progress_inner.set_phase(1, 0);
            let discovered = match scanner::discovery::discover_photos(&path, &progress_inner) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[scan] Skipping {}: {}", path_str, e);
                    continue;
                }
            };

            if discovered.is_empty() {
                continue;
            }

            let folder_count = discovered.len();
            progress_inner.set_phase(2, folder_count);

            // Phase 2: EXIF
            let progress_clone = progress_inner.clone();
            let enriched = scanner::exif::enrich_parallel(discovered, move |parsed| {
                progress_clone.set_parsed(parsed);
            });

            // Filter
            let filter = scanner::filter::PhotoFilter::default();
            let result: Vec<PhotoMeta> = enriched
                .into_iter()
                .filter(|p| filter.passes(p))
                .collect();

            let kept = result.len();
            progress_inner.add_photos_so_far(kept);
            all.extend(result);
        }

        // Deduplicate: content hash (Layer 1) + burst shots (Layer 2)
        all = scanner::dedup::dedup(all);

        Ok(all)
    })
    .await
    .map_err(|e| format!("Scan task failed: {}", e))??;

    progress.mark_done();
    let count = all_photos.len() as u64;

    app_clone.emit("scan_complete", serde_json::json!({
        "total_photos": count,
        "status": "done"
    })).map_err(|e| e.to_string())?;

    println!("[scan] Auto-scan complete: {} photos from {} paths", count, total);
    Ok(all_photos)
}

/// Collect default scan paths on Windows.
/// C drive: only user profile folders (Pictures, Desktop, Downloads, Documents).
/// Non-C drives: enumerate first-level subdirectories to avoid root permission issues.
/// C drive is scanned LAST (after all D-Z drives).
fn get_default_scan_paths() -> Vec<String> {
    let mut paths = Vec::new();

    // 1. Non-C drives: enumerate first-level subdirectories
    for letter in 'D'..='Z' {
        let root = format!("{}:\\", letter);
        if std::path::Path::new(&root).exists() {
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    if let Ok(ft) = entry.file_type() {
                        if ft.is_dir() {
                            let name = entry.file_name().to_string_lossy().to_lowercase();
                            if is_system_dir_name(&name) {
                                continue;
                            }
                            paths.push(entry.path().to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
    }

    // 2. User profile photo folders (C drive whitelist) — scanned LAST
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let user_dirs = [
            format!("{}\\Pictures", profile),
            format!("{}\\Desktop", profile),
            format!("{}\\Downloads", profile),
            format!("{}\\Documents", profile),
        ];

        for p in &user_dirs {
            if std::path::Path::new(p).exists() && !paths.contains(p) {
                paths.push(p.clone());
            }
        }

        // Also scan user-created folders directly under user profile
        if let Ok(entries) = std::fs::read_dir(&profile) {
            for entry in entries.flatten() {
                if let Ok(ft) = entry.file_type() {
                    if ft.is_dir() {
                        let name = entry.file_name().to_string_lossy().to_lowercase();
                        if is_user_profile_skip_dir(&name) {
                            continue;
                        }
                        let sub = entry.path().to_string_lossy().to_string();
                        if !paths.contains(&sub) {
                            paths.push(sub);
                        }
                    }
                }
            }
        }
    }

    paths
}

/// System-like directory names at non-C drive root level — skip these.
fn is_system_dir_name(name: &str) -> bool {
    matches!(
        name,
        "system volume information"
        | "$recycle.bin"
        | "windows"
        | "program files"
        | "program files (x86)"
        | "programdata"
        | "perflogs"
        | "recovery"
        | "boot"
        | "efi"
        | "intel"
        | "amd"
        | "nvidia"
        | "nvidia corporation"
        | "msocache"
    )
}

/// Directories under USERPROFILE that should be skipped — not personal photos.
fn is_user_profile_skip_dir(name: &str) -> bool {
    matches!(
        name,
        "appdata"
        | "application data"
        | "local settings"
        | "cookies"
        | "sendto"
        | "templates"
        | "recent"
        | "start menu"
        | "nethood"
        | "printhood"
        | "localsql"
    ) || name.starts_with('.')
        || name.starts_with("ntuser")
}

/// Get count of photos in local SQLite.
#[tauri::command]
pub async fn get_photo_count(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
) -> Result<u64, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    let count: u64 = conn
        .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
        .unwrap_or(0);
    Ok(count)
}

/// Get stored auth token from local SQLite.
#[tauri::command]
pub async fn get_auth_token(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
) -> Result<Option<String>, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    let token: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'auth_token'",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(token)
}

/// Save auth token to local SQLite.
#[tauri::command]
pub async fn save_auth_token(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    token: String,
) -> Result<(), String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('auth_token', ?1)",
        [&token],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Clear auth token (sign out).
#[tauri::command]
pub async fn clear_auth_token(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
) -> Result<(), String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM settings WHERE key = 'auth_token'",
        [],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Delete a single photo and its annotation from local DB. Does NOT delete the original file.
#[tauri::command]
pub async fn delete_photo(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    file_path_hash: String,
) -> Result<(), String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;

    conn.execute(
        "DELETE FROM annotations WHERE file_path_hash = ?1",
        rusqlite::params![file_path_hash],
    )
    .map_err(|e| e.to_string())?;

    conn.execute(
        "DELETE FROM photos WHERE file_path_hash = ?1",
        rusqlite::params![file_path_hash],
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}

/// Delete all local data (DB + thumbnails) and sign out.
#[tauri::command]
pub async fn clear_local_data(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
) -> Result<(), String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let db_path = app_dir.join("thatday.db");
    let thumb_dir = app_dir.join("thumbnails");

    // Delete DB
    if db_path.exists() {
        // Also delete WAL/SHM files
        for ext in &["", "-wal", "-shm"] {
            let p = app_dir.join(format!("thatday.db{}", ext));
            let _ = std::fs::remove_file(&p);
        }
    }

    // Delete thumbnails
    if thumb_dir.exists() {
        let _ = std::fs::remove_dir_all(&thumb_dir);
    }

    Ok(())
}

// ── Annotation commands ──

/// Save scanned photo metadata to local SQLite (called after scan_directory).
/// Uses a transaction so partial failures roll back completely.
#[tauri::command]
pub async fn save_scanned_photos(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    photos: Vec<PhotoMeta>,
) -> Result<u64, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;

    println!("[save_scanned_photos] received {} photos", photos.len());
    // Speed up bulk insert
    conn.execute_batch("PRAGMA synchronous=OFF; PRAGMA journal_mode=MEMORY;")
        .map_err(|e| format!("PRAGMA error: {}", e))?;
    conn.execute("BEGIN", [])
        .map_err(|e| format!("BEGIN transaction error: {}", e))?;

    let result = (|| -> Result<u64, String> {
        let mut saved = 0u64;
        for p in &photos {
            let ts: Option<i64> = parse_timestamp(&p.timestamp);
            if ts.is_none() {
                eprintln!("[save] WARN: parse_timestamp FAILED for {} (timestamp='{}')", p.file_name, p.timestamp);
            }
            let file_size = std::fs::metadata(&p.file_path)
                .map(|m| m.len() as i64)
                .ok();
            let scene_tags_json = if p.scene_tags.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&p.scene_tags).unwrap_or_default())
            };
            conn.execute(
                "INSERT OR REPLACE INTO photos (file_path_hash, file_path, file_name, file_size, taken_at, scene_tags, thumbnail_path, timestamp_source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![p.file_path_hash, p.file_path, p.file_name, file_size, ts, scene_tags_json, p.thumbnail_key, p.timestamp_source],
            )
            .map_err(|e| format!("DB insert error for {}: {}", p.file_name, e))?;
            saved += 1;
        }
        Ok(saved)
    })();

    match result {
        Ok(n) => {
            conn.execute("COMMIT", []).map_err(|e| format!("COMMIT error: {}", e))?;
            let _ = conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA journal_mode=WAL;");
            println!("[save_scanned_photos] COMMIT success, saved {}", n);
            Ok(n)
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", []);
            eprintln!("[save_scanned_photos] ERROR: {}", e);
            Err(e)
        }
    }
}

/// Get photos for a month-day (MM-DD) across all years within the ThatDay window,
/// with their annotations (LEFT JOIN on the same month-day).
/// Photos are ordered by taken_at ASC (oldest first → newest last).
#[tauri::command]
pub async fn get_photos_by_date(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    date: String,
) -> Result<Vec<PhotoWithAnnotation>, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;

    let (window_start, window_end) = year_window_bounds();

    println!(
        "[get_photos_by_date] date={} window_start={} window_end={}",
        date, window_start, window_end
    );

    // date is now "MM-DD" — match across all years within the window
    let mut stmt = conn
        .prepare(
            "SELECT p.file_path_hash, p.file_name, p.file_path, p.file_size, p.taken_at,
                    p.gps_lat, p.gps_lon, p.thumbnail_path, p.scene_tags,
                    p.timestamp_source,
                    a.who, a.where_place, a.event, a.sync_status
             FROM photos p
             LEFT JOIN annotations a ON p.file_path_hash = a.file_path_hash AND a.calendar_date = ?1
             WHERE strftime('%m-%d', p.taken_at, 'unixepoch') = ?1
               AND p.taken_at >= ?2 AND p.taken_at <= ?3
             ORDER BY p.taken_at ASC",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map(rusqlite::params![date, window_start, window_end], |row| {
            Ok(PhotoWithAnnotation {
                file_path_hash: row.get(0)?,
                file_name: row.get(1)?,
                file_path: row.get(2)?,
                file_size: row.get(3)?,
                taken_at: row.get(4)?,
                gps_lat: row.get(5)?,
                gps_lon: row.get(6)?,
                thumbnail_path: row.get(7)?,
                scene_tags: {
                    let raw: Option<String> = row.get(8)?;
                    match raw {
                        Some(s) if !s.is_empty() => serde_json::from_str(&s).unwrap_or_default(),
                        _ => Vec::new(),
                    }
                },
                timestamp_source: row.get(9)?,
                who: row.get(10)?,
                where_place: row.get(11)?,
                event: row.get(12)?,
                sync_status: row.get(13)?,
            })
        })
        .map_err(|e| e.to_string())?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| e.to_string())?);
    }

    println!(
        "[get_photos_by_date] date={} returned {} rows",
        date,
        result.len()
    );

    Ok(result)
}

/// Upsert an annotation for one photo. Sets sync_status = 'local'.
#[tauri::command]
pub async fn save_annotation(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    input: AnnotationInput,
) -> Result<(), String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;

    // Check if annotation exists for this photo+date
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM annotations WHERE file_path_hash = ?1 AND calendar_date = ?2",
            rusqlite::params![input.file_path_hash, input.calendar_date],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if exists {
        conn.execute(
            "UPDATE annotations SET who = ?1, where_place = ?2, event = ?3, sync_status = 'local',
                    created_at = datetime('now')
             WHERE file_path_hash = ?4 AND calendar_date = ?5",
            rusqlite::params![
                input.who,
                input.where_place,
                input.event,
                input.file_path_hash,
                input.calendar_date
            ],
        )
        .map_err(|e| e.to_string())?;
    } else {
        conn.execute(
            "INSERT INTO annotations (calendar_date, file_path_hash, who, where_place, event, sync_status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'local')",
            rusqlite::params![
                input.calendar_date,
                input.file_path_hash,
                input.who,
                input.where_place,
                input.event,
            ],
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// Autocomplete 'who' values from annotation history.
#[tauri::command]
pub async fn autocomplete_who(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    prefix: String,
) -> Result<Vec<String>, String> {
    autocomplete_field(app, "who", &prefix)
}

/// Autocomplete 'where' values from annotation history.
#[tauri::command]
pub async fn autocomplete_where(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    prefix: String,
) -> Result<Vec<String>, String> {
    autocomplete_field(app, "where_place", &prefix)
}

fn autocomplete_field(
    app: tauri::AppHandle,
    field: &str,
    prefix: &str,
) -> Result<Vec<String>, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;

    let col = match field {
        "who" => "who",
        "where_place" => "where_place",
        _ => return Err(format!("Invalid autocomplete field: {}", field)),
    };

    let sql = format!(
        "SELECT DISTINCT {col} FROM annotations WHERE {col} LIKE ?1 AND {col} != '' AND {col} IS NOT NULL
         ORDER BY created_at DESC LIMIT 8",
        col = col
    );

    let search = if prefix.is_empty() {
        "%".to_string()
    } else {
        format!("{}%", prefix)
    };

    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![search], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;

    let mut result = Vec::new();
    for row in rows {
        if let Ok(val) = row {
            result.push(val);
        }
    }
    Ok(result)
}

/// Inner helper: get all distinct MM-DD dates that have photos within the window.
fn get_available_dates_inner(
    conn: &rusqlite::Connection,
    window_start: i64,
    window_end: i64,
) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT strftime('%m-%d', taken_at, 'unixepoch') as md
             FROM photos
             WHERE taken_at IS NOT NULL
               AND taken_at >= ?1 AND taken_at <= ?2
             ORDER BY md ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![window_start, window_end], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut dates = Vec::new();
    for r in rows {
        dates.push(r.map_err(|e| e.to_string())?);
    }
    Ok(dates)
}

/// Get all distinct month-days (MM-DD) that have photos within the ThatDay year window.
/// Sorted chronologically by month-day (Jan→Dec).
#[tauri::command]
pub async fn get_available_dates(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    let (window_start, window_end) = year_window_bounds();
    let dates = get_available_dates_inner(&conn, window_start, window_end)?;

    println!("[get_available_dates] window=({}, {}) returned {} dates: {:?}", window_start, window_end, dates.len(), &dates[..10.min(dates.len())]);

    Ok(dates)
}

/// Build seal data: all annotations + photo info for today, ready to submit to Worker.
#[tauri::command]
pub async fn get_seal_data(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    date: String,
) -> Result<serde_json::Value, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;

    let mut stmt = conn
        .prepare(
            "SELECT p.file_name, p.taken_at, p.gps_lat, p.gps_lon,
                    p.scene_tags,
                    a.who, a.where_place, a.event
             FROM photos p
             INNER JOIN annotations a ON p.file_path_hash = a.file_path_hash
             WHERE a.calendar_date = ?1 AND a.sync_status = 'local'
             ORDER BY p.taken_at ASC",
        )
        .map_err(|e| e.to_string())?;

    let mut photos: Vec<serde_json::Value> = Vec::new();
    let rows = stmt
        .query_map(rusqlite::params![date], |row| {
            let taken_at: Option<i64> = row.get(1)?;
            let gps_lat: Option<f64> = row.get(2)?;
            let gps_lon: Option<f64> = row.get(3)?;
            let scene_tags_raw: Option<String> = row.get(4)?;

            let time_str = taken_at
                .map(|t| {
                    chrono::DateTime::from_timestamp(t, 0)
                        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
                        .unwrap_or_default()
                })
                .unwrap_or_default();

            let gps = if gps_lat.is_some() && gps_lon.is_some() {
                serde_json::json!({"lat": gps_lat, "lon": gps_lon})
            } else {
                serde_json::Value::Null
            };

            // DB stores scene_tags as JSON array ["tag1","tag2"].
            // Worker expects Record<string, boolean> → {"tag1":true,"tag2":true}.
            let scene_tags: serde_json::Value = scene_tags_raw
                .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                .map(|tags| {
                    let mut map = serde_json::Map::new();
                    for t in tags {
                        map.insert(t, serde_json::Value::Bool(true));
                    }
                    serde_json::Value::Object(map)
                })
                .unwrap_or(serde_json::Value::Null);

            Ok(serde_json::json!({
                "time": time_str,
                "gps": gps,
                "who": row.get::<_, Option<String>>(5)?,
                "where": row.get::<_, Option<String>>(6)?,
                "event": row.get::<_, Option<String>>(7)?,
                "scene_tags": scene_tags,
            }))
        })
        .map_err(|e| e.to_string())?;

    for row in rows {
        photos.push(row.map_err(|e| e.to_string())?);
    }

    Ok(serde_json::json!({
        "calendar_date": date,
        "photos": photos
    }))
}

/// Mark annotations as synced after successful Worker upload.
#[tauri::command]
pub async fn mark_annotations_synced(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    date: String,
) -> Result<u64, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    let n = conn
        .execute(
            "UPDATE annotations SET sync_status = 'synced' WHERE calendar_date = ?1 AND sync_status = 'local'",
            rusqlite::params![date],
        )
        .map_err(|e| e.to_string())?;
    Ok(n as u64)
}

/// Reset local annotations sync_status back to 'local' for a date.
/// Used when user wants to re-seal a previously sealed date.
#[tauri::command]
pub async fn reset_annotations_local(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    date: String,
) -> Result<u64, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    let n = conn
        .execute(
            "UPDATE annotations SET sync_status = 'local' WHERE calendar_date = ?1",
            rusqlite::params![date],
        )
        .map_err(|e| e.to_string())?;
    Ok(n as u64)
}

/// Generate thumbnails + classify for all photos in the DB that are missing either.
/// Useful for migrating existing photos after upgrading the app.
#[tauri::command]
pub async fn generate_missing_thumbnails(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<u64, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = crate::db::get_db(&app_dir).map_err(|e| e.to_string())?;
    let thumb_dir = app_dir.join("thumbnails");

    // RAII guard for classifier
    struct ClassifierGuard<'a> {
        state: &'a tauri::State<'a, AppState>,
        classifier: Option<crate::scene_classifier::SceneClassifier>,
    }
    impl<'a> Drop for ClassifierGuard<'a> {
        fn drop(&mut self) {
            let taken = self.classifier.take();
            if let Ok(mut guard) = self.state.classifier.lock() {
                *guard = taken;
            }
        }
    }

    let classifier = state.classifier.lock().unwrap().take();
    let guard = ClassifierGuard { state: &state, classifier };

    // Find photos without thumbnails OR without scene_tags
    let mut stmt = conn
        .prepare(
            "SELECT file_path_hash, file_path, scene_tags FROM photos
             WHERE thumbnail_path IS NULL OR thumbnail_path = ''
                OR scene_tags IS NULL OR scene_tags = ''",
        )
        .map_err(|e| e.to_string())?;

    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        return Ok(0);
    }

    let mut processed = 0u64;
    for (hash, path, existing_tags) in &rows {
        let needs_classify = existing_tags.as_ref().map(|s| s.is_empty()).unwrap_or(true);
        let needs_thumb = true; // always try thumb (idempotent - generate() skips if exists)

        let tags_json: Option<String> = if needs_classify {
            if let Some(ref classifier) = guard.classifier {
                let tags = classifier.classify(path);
                if tags.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&tags).unwrap_or_default())
                }
            } else {
                None
            }
        } else {
            existing_tags.clone()
        };

        let thumb_path = if needs_thumb {
            crate::scanner::thumbnail::generate(path, hash, &thumb_dir)
        } else {
            None
        };

        conn.execute(
            "UPDATE photos SET scene_tags = COALESCE(?1, scene_tags), thumbnail_path = COALESCE(?2, thumbnail_path) WHERE file_path_hash = ?3",
            rusqlite::params![tags_json, thumb_path, hash],
        )
        .map_err(|e| e.to_string())?;

        processed += 1;
    }

    Ok(processed)
}

/// Read a thumbnail file and return it as a base64 data URL.
/// Bypasses the Tauri asset protocol entirely — no `asset.localhost` dependency.
#[tauri::command]
pub async fn read_thumbnail_base64(
    app: tauri::AppHandle,
    hash: String,
) -> Result<Option<String>, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let thumb_path = app_dir.join("thumbnails").join(format!("{}.jpg", hash));

    if !thumb_path.exists() {
        return Ok(None);
    }

    let data = std::fs::read(&thumb_path).map_err(|e| format!("Failed to read thumbnail: {}", e))?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
    Ok(Some(format!("data:image/jpeg;base64,{}", b64)))
}

/// Inner helper: classify + thumbnail for photos of one MM-DD date.
/// Does NOT own the classifier — caller manages its lifetime.
/// `serial = true` → one photo at a time (low CPU, for warm_cache).
/// `serial = false` → parallel threads (fast, for interactive browsing).
fn classify_date_inner(
    conn: &rusqlite::Connection,
    classifier: Option<&crate::scene_classifier::SceneClassifier>,
    date: &str,
    window_start: i64,
    window_end: i64,
    thumb_dir: &std::path::Path,
    serial: bool,
) -> Result<u64, String> {
    let mut stmt = conn
        .prepare(
            "SELECT file_path_hash, file_path FROM photos
             WHERE strftime('%m-%d', taken_at, 'unixepoch') = ?1
               AND taken_at >= ?2 AND taken_at <= ?3
               AND (scene_tags IS NULL OR scene_tags = '' OR thumbnail_path IS NULL OR thumbnail_path = '')
             ORDER BY taken_at ASC",
        )
        .map_err(|e| e.to_string())?;

    let pending: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![date, window_start, window_end], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

    if pending.is_empty() {
        return Ok(0);
    }

    if serial {
        // ── Serial: one photo at a time, minimal CPU ──
        let mut classified = 0u64;
        for (hash, path) in &pending {
            let tags: Vec<String> = if let Some(c) = classifier {
                c.classify(path)
            } else {
                Vec::new()
            };
            let tags_json = if tags.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&tags).unwrap_or_default())
            };
            let thumb_path = crate::scanner::thumbnail::generate(path, hash, thumb_dir);
            conn.execute(
                "UPDATE photos SET scene_tags = ?1, thumbnail_path = ?2 WHERE file_path_hash = ?3",
                rusqlite::params![tags_json, thumb_path, hash],
            )
            .map_err(|e| e.to_string())?;
            classified += 1;
        }
        Ok(classified)
    } else {
        // ── Parallel, chunked: max 8 concurrent classifications to avoid CPU storm ──
        const CHUNK: usize = 8;
        let n = pending.len();
        let all_results = std::sync::Mutex::new(vec![(None::<String>, None::<String>); n]);

        for chunk_start in (0..n).step_by(CHUNK) {
            let chunk_end = std::cmp::min(chunk_start + CHUNK, n);
            std::thread::scope(|s| {
                for i in chunk_start..chunk_end {
                    let (hash, path) = &pending[i];
                    let results = &all_results;
                    let thumb_dir = thumb_dir;
                    s.spawn(move || {
                        let tags: Vec<String> = if let Some(c) = classifier {
                            c.classify(path)
                        } else {
                            Vec::new()
                        };
                        let tags_json = if tags.is_empty() {
                            None
                        } else {
                            Some(serde_json::to_string(&tags).unwrap_or_default())
                        };
                        let thumb_path = crate::scanner::thumbnail::generate(path, hash, thumb_dir);
                        results.lock().unwrap()[i] = (tags_json, thumb_path);
                    });
                }
            });
        }
        // Sequential DB updates
        let mut classified = 0u64;
        let results = all_results.lock().unwrap();
        for (i, (hash, _path)) in pending.iter().enumerate() {
            let (tags_json, thumb_path) = &results[i];
            conn.execute(
                "UPDATE photos SET scene_tags = ?1, thumbnail_path = ?2 WHERE file_path_hash = ?3",
                rusqlite::params![tags_json, thumb_path, hash],
            )
            .map_err(|e| e.to_string())?;
            classified += 1;
        }
        Ok(classified)
    }
}

/// Classify and generate thumbnails for all photos on a given month-day (MM-DD)
/// across the ThatDay year window. Called lazily when browsing a date.
#[tauri::command]
pub async fn classify_date(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    date: String,
) -> Result<u64, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let thumb_dir = app_dir.join("thumbnails");

    // ── Take classifier with RAII guard ──
    struct ClassifierGuard<'a> {
        state: &'a tauri::State<'a, AppState>,
        classifier: Option<crate::scene_classifier::SceneClassifier>,
    }
    impl<'a> Drop for ClassifierGuard<'a> {
        fn drop(&mut self) {
            let taken = self.classifier.take();
            if let Ok(mut guard) = self.state.classifier.lock() {
                *guard = taken;
            }
        }
    }

    let classifier = state.classifier.lock().unwrap().take();
    let guard = ClassifierGuard { state: &state, classifier };

    let (window_start, window_end) = year_window_bounds();
    let conn = crate::db::get_db(&app_dir).map_err(|e| e.to_string())?;

    classify_date_inner(&conn, guard.classifier.as_ref(), &date, window_start, window_end, &thumb_dir, false)
}

/// Compute the 7-day sliding window (±3 calendar days) around a MM-DD date.
fn compute_7day_set(date: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    // Parse as arbitrary non-leap year (2024)
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&format!("2024-{}", date), "%Y-%m-%d") {
        for offset in -3i64..=3i64 {
            let nd = d + chrono::Duration::days(offset);
            set.insert(nd.format("%m-%d").to_string());
        }
    }
    set
}

/// Warm the thumbnail + scene-tag cache after a scan.
/// Priority: today → 7-day window → rest (recent first).
/// Emits `warm_cache_progress` and `warm_cache_complete` events.
#[tauri::command]
pub async fn warm_cache(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    today: String,
) -> Result<serde_json::Value, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let thumb_dir = app_dir.join("thumbnails");
    let (window_start, window_end) = year_window_bounds();

    // ── Take classifier once for the whole warm-up ──
    struct ClassifierGuard<'a> {
        state: &'a tauri::State<'a, AppState>,
        classifier: Option<crate::scene_classifier::SceneClassifier>,
    }
    impl<'a> Drop for ClassifierGuard<'a> {
        fn drop(&mut self) {
            let taken = self.classifier.take();
            if let Ok(mut guard) = self.state.classifier.lock() {
                *guard = taken;
            }
        }
    }

    let classifier = state.classifier.lock().unwrap().take();
    let guard = ClassifierGuard { state: &state, classifier };
    let classifier_ref = guard.classifier.as_ref();

    let conn = crate::db::get_db(&app_dir).map_err(|e| e.to_string())?;
    let all_dates = get_available_dates_inner(&conn, window_start, window_end)?;

    // Partition by priority
    let window_set = compute_7day_set(&today);
    let mut prio1: Vec<String> = Vec::new(); // today
    let mut prio2: Vec<String> = Vec::new(); // 7-day window
    let mut prio3: Vec<String> = Vec::new(); // rest

    for d in &all_dates {
        if *d == today {
            prio1.push(d.clone());
        } else if window_set.contains(d.as_str()) {
            prio2.push(d.clone());
        } else {
            prio3.push(d.clone());
        }
    }
    // prio3 is sorted ascending (Jan→Dec); reverse → recent-first
    prio3.reverse();

    let mut ordered_dates = Vec::new();
    ordered_dates.append(&mut prio1);
    ordered_dates.append(&mut prio2);
    ordered_dates.append(&mut prio3);

    let total_dates = ordered_dates.len();
    let mut total_processed = 0u64;

    let _ = app.emit("warm_cache_progress", serde_json::json!({
        "total_dates": total_dates,
        "completed": 0,
        "total_photos_processed": 0,
        "current_date": ""
    }));

    for (i, date) in ordered_dates.iter().enumerate() {
        // Serial mode: one photo at a time, minimal CPU — the user won't notice
        let n = classify_date_inner(
            &conn,
            classifier_ref,
            date,
            window_start,
            window_end,
            &thumb_dir,
            true,
        )?;
        total_processed += n;

        let _ = app.emit("warm_cache_progress", serde_json::json!({
            "total_dates": total_dates,
            "completed": i + 1,
            "total_photos_processed": total_processed,
            "current_date": date
        }));
    }

    app.emit("warm_cache_complete", serde_json::json!({
        "total_dates_processed": total_dates,
        "total_photos_processed": total_processed
    })).map_err(|e| e.to_string())?;

    println!("[warm_cache] done: {} photos across {} dates", total_processed, total_dates);

    Ok(serde_json::json!({
        "total_dates": total_dates,
        "total_photos_processed": total_processed
    }))
}

// ── TTS: Text-to-speech via bundled Piper ──

/// Generate spoken WAV audio from story text.
#[tauri::command]
pub async fn speak_story(
    app: tauri::AppHandle,
    date: String,
    text: String,
) -> Result<String, String> {
    let resource_dir = app.path().resource_dir().map_err(|e| e.to_string())?;
    let custom_dir = read_output_dir_setting(&app).ok();
    let out_path = crate::tts::speak(
        &resource_dir, &date, &text,
        custom_dir.as_deref().map(|p| std::path::Path::new(p)),
    )?;
    Ok(out_path.to_string_lossy().to_string())
}

// ── Cloud upload ──

const API_BASE: &str = "https://api.thatday.vip";

/// Upload PDF and WAV files to Worker → R2 storage.
#[tauri::command]
pub async fn upload_media(
    app: tauri::AppHandle,
    date: String,
    pdf_path: String,
    wav_path: String,
) -> Result<serde_json::Value, String> {
    let token = {
        let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
        let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT value FROM settings WHERE key = 'auth_token'",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .map_err(|e| format!("Failed to get auth token: {}", e))?
        .ok_or_else(|| "Not authenticated".to_string())?
    };

    let pdf_bytes = std::fs::read(&pdf_path)
        .map_err(|e| format!("Failed to read PDF: {}", e))?;
    let wav_bytes = std::fs::read(&wav_path)
        .map_err(|e| format!("Failed to read WAV: {}", e))?;

    let pdf_name = format!("{}.pdf", date);
    let wav_name = format!("{}.wav", date);

    let pdf_part = reqwest::multipart::Part::bytes(pdf_bytes)
        .file_name(pdf_name)
        .mime_str("application/pdf")
        .map_err(|e| e.to_string())?;
    let wav_part = reqwest::multipart::Part::bytes(wav_bytes)
        .file_name(wav_name)
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;

    let form = reqwest::multipart::Form::new()
        .part("pdf", pdf_part)
        .part("audio", wav_part)
        .text("date", date);

    let client = reqwest::Client::new();
    let res = client
        .post(format!("{}/media/upload", API_BASE))
        .header("Authorization", format!("Bearer {}", token))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("Upload request failed: {}", e))?;

    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        return Err(format!("Upload failed: {} — {}", status, text));
    }

    let json: serde_json::Value = res.json().await.map_err(|e| format!("Bad upload response: {}", e))?;
    Ok(json)
}

// ── Output directory setting ──

/// Helper: read the custom output_dir from settings table.
fn read_output_dir_setting(app: &tauri::AppHandle) -> Result<String, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'output_dir'",
        [],
        |r| r.get::<_, Option<String>>(0),
    )
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "not set".to_string())
}

/// Get the configured output directory for PDF + audio files.
/// Falls back to Documents/That Day Stories if not set.
#[tauri::command]
pub async fn get_output_dir(app: tauri::AppHandle) -> Result<String, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    let custom: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'output_dir'",
            [],
            |r| r.get(0),
        )
        .ok();
    match custom {
        Some(p) if !p.is_empty() => Ok(p),
        _ => Ok(crate::tts::default_output_dir().to_string_lossy().to_string()),
    }
}

/// Set a custom output directory. Validates that the path exists.
#[tauri::command]
pub async fn set_output_dir(
    app: tauri::AppHandle,
    dir_path: String,
) -> Result<(), String> {
    let path = std::path::Path::new(&dir_path);
    if !path.exists() {
        return Err(format!("Folder does not exist: {}", dir_path));
    }
    if !path.is_dir() {
        return Err("Path is not a folder".into());
    }
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('output_dir', ?1)",
        rusqlite::params![dir_path],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ── Local Stories ──

/// Save a story to local SQLite (called after Save generates story via Worker).
#[tauri::command]
pub async fn save_story(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    date: String,
    title: String,
    content: String,
) -> Result<(), String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT OR REPLACE INTO stories (calendar_date, title, content, created_at) VALUES (?1, ?2, ?3, datetime('now'))",
        rusqlite::params![date, title, content],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Get all stories whose calendar_date ≤ today (0:00 release).
#[tauri::command]
pub async fn get_stories(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<Vec<serde_json::Value>, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let lim = limit.unwrap_or(50);

    let mut stmt = conn
        .prepare(
            "SELECT calendar_date, title, content, created_at FROM stories WHERE calendar_date <= ?1 ORDER BY calendar_date DESC LIMIT ?2"
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map(rusqlite::params![today, lim], |row| {
            Ok(serde_json::json!({
                "calendar_date": row.get::<_, String>(0)?,
                "title": row.get::<_, String>(1)?,
                "content": row.get::<_, String>(2)?,
                "created_at": row.get::<_, Option<String>>(3)?,
            }))
        })
        .map_err(|e| e.to_string())?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| e.to_string())?);
    }
    Ok(result)
}

/// Get a single local story by date.
#[tauri::command]
pub async fn get_story_local(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    date: String,
) -> Result<Option<serde_json::Value>, String> {
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = get_db(&app_dir).map_err(|e| e.to_string())?;

    let result = conn
        .query_row(
            "SELECT calendar_date, title, content, created_at FROM stories WHERE calendar_date = ?1",
            rusqlite::params![date],
            |row| {
                Ok(serde_json::json!({
                    "calendar_date": row.get::<_, String>(0)?,
                    "title": row.get::<_, String>(1)?,
                    "content": row.get::<_, String>(2)?,
                    "created_at": row.get::<_, Option<String>>(3)?,
                }))
            },
        )
        .ok();

    Ok(result)
}

// ── PDF Generation ──

/// Generate a PDF keepsake for a given date. Returns the local file path.
/// Skips generation if the file already exists (just returns the path).
#[tauri::command]
pub async fn generate_pdf(
    app: tauri::AppHandle,
    date: String,
    title: String,
    content: String,
) -> Result<String, String> {
    let custom_dir = read_output_dir_setting(&app).ok();
    let out_dir = custom_dir
        .as_deref()
        .map(|p| std::path::PathBuf::from(p))
        .unwrap_or_else(|| crate::tts::default_output_dir());
    let expected = out_dir.join(format!("{}.pdf", date));
    if expected.exists() {
        return Ok(expected.to_string_lossy().to_string());
    }
    let app_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let resource_dir = app.path().resource_dir().map_err(|e| e.to_string())?;
    let out_path = crate::pdf::generate(
        &app_dir, &resource_dir, &date, &title, &content,
        custom_dir.as_deref().map(|p| std::path::Path::new(p)),
    )?;
    Ok(out_path.to_string_lossy().to_string())
}

/// Return the PDF path for a date (whether it exists or not).
#[tauri::command]
pub async fn get_pdf_path(app: tauri::AppHandle, date: String) -> Result<String, String> {
    let custom_dir = read_output_dir_setting(&app).ok();
    let out_dir = custom_dir
        .as_deref()
        .map(|p| std::path::PathBuf::from(p))
        .unwrap_or_else(|| crate::tts::default_output_dir());
    Ok(out_dir.join(format!("{}.pdf", date)).to_string_lossy().to_string())
}

/// Open a local file or folder with the system default program (bypasses Tauri URL validation).
#[tauri::command]
pub async fn open_path(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if p.is_dir() {
        std::process::Command::new("explorer")
            .arg(p)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    } else {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &path])
            .spawn()
            .map_err(|e| format!("Failed to open file: {}", e))?;
    }
    Ok(())
}

// ── Helpers ──

fn parse_timestamp(s: &str) -> Option<i64> {
    use chrono::NaiveDateTime;
    // Try ISO 8601 with T separator
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive_to_local_ts(dt));
    }
    // Try space separator
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(naive_to_local_ts(dt));
    }
    // Try date only
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0)?;
        return Some(naive_to_local_ts(dt));
    }
    None
}

/// Convert naive local datetime to Unix timestamp (local timezone, not UTC).
fn naive_to_local_ts(dt: chrono::NaiveDateTime) -> i64 {
    match dt.and_local_timezone(chrono::Local) {
        chrono::LocalResult::Single(local_dt) => local_dt.timestamp(),
        _ => dt.and_utc().timestamp(), // fallback: treat as UTC
    }
}
