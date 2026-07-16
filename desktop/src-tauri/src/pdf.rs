//! PDF generation — keepsake export for That Day stories.
//!
//! Layout:
//!   Page 1: Cover (date + title + That Day logo)
//!   Pages 2+: Per-year sections (photos row + story text)
//!
//! Output: Documents/That Day Stories/YYYY-MM-DD.pdf

use crate::db::get_db;
use crate::scanner::dedup::{compute_ahash, hamming_distance};
use genpdf::{elements, fonts, style, Alignment, Element, SimplePageDecorator};
use std::path::{Path, PathBuf};

const BRAND: &str = "That Day";
const BRAND_URL: &str = "thatday.vip";
const LOGO_PNG: &[u8] = include_bytes!("../icons/icon.png");

pub struct PdfPhoto {
    pub file_path_hash: String,
    pub taken_at: Option<i64>,
    pub who: Option<String>,
    pub where_place: Option<String>,
    pub event: Option<String>,
    pub file_name: String,
}

/// Generate a PDF keepsake for a given date.
pub fn generate(
    app_dir: &Path,
    resource_dir: &Path,
    date: &str,
    title: &str,
    content: &str,
    output_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    // ── Load font ──
    let search_dirs = [
        resource_dir.to_path_buf(),
        resource_dir.parent().map(|p| p.join("resources")).unwrap_or_default(),
        resource_dir.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        std::env::current_dir().unwrap_or_default(),
        std::env::current_dir().map(|p| p.join("resources")).unwrap_or_default(),
    ];

    let font_dir = search_dirs
        .iter()
        .map(|d| d.join("fonts"))
        .find(|d| d.join("CrimsonText-Regular.ttf").exists());

    let font_dir = match font_dir {
        Some(d) => d,
        None => {
            let tried: Vec<String> = search_dirs
                .iter()
                .map(|d| d.join("fonts").join("CrimsonText-Regular.ttf").display().to_string())
                .collect();
            return Err(format!(
                "Crimson Text font not found. Tried:\n  {}",
                tried.join("\n  ")
            ));
        }
    };

    let regular_data =
        std::fs::read(font_dir.join("CrimsonText-Regular.ttf")).map_err(|e| format!("Font: {}", e))?;
    let regular = fonts::FontData::new(regular_data, None).map_err(|e| format!("Font: {}", e))?;

    let bold = std::fs::read(font_dir.join("CrimsonText-Bold.ttf"))
        .ok()
        .and_then(|d| fonts::FontData::new(d, None).ok());
    let italic = std::fs::read(font_dir.join("CrimsonText-Italic.ttf"))
        .ok()
        .and_then(|d| fonts::FontData::new(d, None).ok());

    let reg_clone = regular.clone();
    let font_family = fonts::FontFamily {
        regular,
        bold: bold.unwrap_or_else(|| reg_clone.clone()),
        italic: italic.unwrap_or_else(|| reg_clone.clone()),
        bold_italic: reg_clone,
    };

    // ── Create document ──
    let mut doc = genpdf::Document::new(font_family);
    doc.set_title(format!("{} — {}", BRAND, date));
    doc.set_paper_size((210.0, 297.0));
    let mut decorator = SimplePageDecorator::new();
    decorator.set_margins(20);
    doc.set_page_decorator(decorator);

    // ── Write logo to temp file (genpdf images must be loaded from path) ──
    let logo_tmp = std::env::temp_dir().join("thatday_logo.pdfgen.png");
    std::fs::write(&logo_tmp, LOGO_PNG).ok();

    // ── Cover page ──
    push_br(&mut doc, 6);
    if logo_tmp.exists() {
        if let Ok(mut img) = elements::Image::from_path(&logo_tmp) {
            img.set_dpi(400.0); // Scale icon to ~65px wide for cover
            doc.push(img);
        }
    }
    push_br(&mut doc, 2);
    push_text(&mut doc, BRAND, 28, Alignment::Center, true);
    push_br(&mut doc, 2);
    push_text(&mut doc, &format_date(date), 14, Alignment::Center, false);
    push_br(&mut doc, 2);
    push_text(&mut doc, title, 11, Alignment::Center, false);
    push_br(&mut doc, 6);
    push_text(&mut doc, BRAND_URL, 9, Alignment::Center, false);
    // Force page break after cover (genpdf has no native PageBreak; fill with blank lines)
    push_br(&mut doc, 28);

    // ── Body ──
    let mut photos = load_photos(app_dir, date)?;
    let thumb_dir = app_dir.join("thumbnails");

    // Dedup burst + visually similar photos (30s window + aHash)
    photos = dedup_photos(photos, &thumb_dir);

    // Brand in top-right of body page
    push_text(&mut doc, BRAND, 10, Alignment::Right, true);
    push_br(&mut doc, 2);

    // Photos (chronological) — page-break every N photos to avoid image cutoff at page bottom
    if !photos.is_empty() {
        push_text(&mut doc, "Photos", 12, Alignment::Left, true);
        push_br(&mut doc, 1);
        let mut photos_on_page: u32 = 0;
        const MAX_PER_PAGE: u32 = 3;
        for photo in &photos {
            if photos_on_page >= MAX_PER_PAGE {
                push_br(&mut doc, 45); // force page break
                photos_on_page = 0;
            }
            let thumb_path = thumb_dir.join(format!("{}.jpg", photo.file_path_hash));
            if thumb_path.exists() {
                if let Ok(mut img) = elements::Image::from_path(&thumb_path) {
                    img.set_dpi(96.0); // scale thumb ~3x
                    doc.push(img);
                    let cap = build_caption(photo);
                    if !cap.is_empty() {
                        push_text(&mut doc, &cap, 8, Alignment::Center, false);
                    }
                    push_br(&mut doc, 2);
                }
            }
            photos_on_page += 1;
        }
    }

    // Story text
    push_br(&mut doc, 1);
    push_text(&mut doc, title, 14, Alignment::Left, true);
    push_br(&mut doc, 2);

    for para in content.split('\n').filter(|p| !p.trim().is_empty()) {
        push_text(&mut doc, para.trim(), 10, Alignment::Left, false);
        push_br(&mut doc, 1);
    }

    // ── Footer ──
    push_br(&mut doc, 2);
    push_text(&mut doc, &format!("{} · {}", BRAND, BRAND_URL), 7, Alignment::Center, false);

    // ── Save ──
    let out_dir = output_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dirs_documents().join("That Day Stories"));
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("Failed to create output directory: {}", e))?;

    let out_path = out_dir.join(format!("{}.pdf", date));
    doc.render_to_file(&out_path)
        .map_err(|e| format!("Failed to render PDF: {}", e))?;

    println!("[pdf] Generated: {}", out_path.display());
    Ok(out_path)
}

// ── Helpers ──

fn dirs_documents() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .map(|p| p.join("Documents"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn push_text(doc: &mut genpdf::Document, text: &str, _size: u8, align: Alignment, bold: bool) {
    let s = if bold {
        style::Style::new().bold()
    } else {
        style::Style::new()
    };
    doc.push(elements::Paragraph::new(text).aligned(align).styled(s));
}

fn push_br(doc: &mut genpdf::Document, count: usize) {
    for _ in 0..count {
        doc.push(elements::Break::new(1));
    }
}

fn load_photos(app_dir: &Path, date: &str) -> Result<Vec<PdfPhoto>, String> {
    let conn = get_db(&app_dir.to_path_buf()).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT p.file_path_hash, p.file_name, p.taken_at,
                    a.who, a.where_place, a.event
             FROM photos p
             LEFT JOIN annotations a ON p.file_path_hash = a.file_path_hash AND a.calendar_date = substr(?1, 6)
             WHERE strftime('%m-%d', p.taken_at, 'unixepoch') = substr(?1, 6)
             ORDER BY p.taken_at ASC",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map(rusqlite::params![date], |row| {
            Ok(PdfPhoto {
                file_path_hash: row.get(0)?,
                file_name: row.get(1)?,
                taken_at: row.get(2)?,
                who: row.get(3)?,
                where_place: row.get(4)?,
                event: row.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;

    let mut photos = Vec::new();
    for r in rows {
        photos.push(r.map_err(|e| e.to_string())?);
    }
    Ok(photos)
}

fn build_caption(photo: &PdfPhoto) -> String {
    let parts: Vec<&str> = [
        photo.who.as_deref().unwrap_or(""),
        photo.where_place.as_deref().unwrap_or(""),
        photo.event.as_deref().unwrap_or(""),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect();

    if parts.is_empty() {
        String::new()
    } else {
        parts.join(" · ")
    }
}

fn format_date(date_str: &str) -> String {
    if let Ok(d) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        d.format("%A, %B %d, %Y").to_string()
    } else {
        date_str.to_string()
    }
}

// ── Photo dedup for PDF display ──

fn dedup_photos(photos: Vec<PdfPhoto>, thumb_dir: &Path) -> Vec<PdfPhoto> {
    if photos.len() <= 1 { return photos; }
    let mut sorted = photos;
    sorted.sort_by_key(|p| p.taken_at.unwrap_or(i64::MAX));

    let mut groups: Vec<Vec<PdfPhoto>> = Vec::new();
    for p in sorted {
        if let Some(last_group) = groups.last_mut() {
            if let (Some(cur_ts), Some(last_ts)) = (p.taken_at, last_group.last().and_then(|lp| lp.taken_at)) {
                if cur_ts - last_ts <= 30 {
                    last_group.push(p);
                    continue;
                }
            }
        }
        groups.push(vec![p]);
    }

    let mut result = Vec::new();
    for group in groups {
        if group.len() <= 1 {
            result.extend(group);
        } else {
            result.extend(dedup_group_a(group, thumb_dir));
        }
    }
    result
}

fn dedup_group_a(group: Vec<PdfPhoto>, thumb_dir: &Path) -> Vec<PdfPhoto> {
    let hashed: Vec<(PdfPhoto, Option<u64>)> = group
        .into_iter()
        .map(|p| {
            let thumb = thumb_dir.join(format!("{}.jpg", p.file_path_hash));
            let h = compute_ahash(&thumb.to_string_lossy());
            (p, h)
        })
        .collect();

    let mut kept = vec![true; hashed.len()];
    for i in 0..hashed.len() {
        if !kept[i] { continue; }
        for j in (i+1)..hashed.len() {
            if !kept[j] { continue; }
            if let (Some(hi), Some(hj)) = (hashed[i].1, hashed[j].1) {
                if hamming_distance(hi, hj) <= 5 {
                    let si = anno_score(&hashed[i].0);
                    let sj = anno_score(&hashed[j].0);
                    if sj > si { kept[i] = false; } else { kept[j] = false; }
                }
            }
        }
    }

    hashed.into_iter().enumerate()
        .filter(|(i, _)| kept[*i])
        .map(|(_, (p, _))| p)
        .collect()
}

fn anno_score(p: &PdfPhoto) -> u32 {
    let mut s = 0u32;
    if p.who.is_some() { s += 1; }
    if p.where_place.is_some() { s += 1; }
    if p.event.is_some() { s += 1; }
    s
}
