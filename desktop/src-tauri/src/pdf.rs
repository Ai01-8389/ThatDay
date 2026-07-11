//! PDF generation — keepsake export for That Day stories.
//!
//! Layout:
//!   Page 1: Cover (date + title + That Day logo)
//!   Pages 2+: Per-year sections (photos row + story text)
//!
//! Output: Documents/That Day Stories/YYYY-MM-DD.pdf

use crate::db::get_db;
use genpdf::{elements, fonts, style, Alignment, Element, SimplePageDecorator};
use std::path::{Path, PathBuf};

const BRAND: &str = "That Day";
const BRAND_URL: &str = "thatday.vip";

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

    // ── Cover page (minimal) ──
    push_br(&mut doc, 10);
    push_text(&mut doc, BRAND, 36, Alignment::Center, true);
    push_br(&mut doc, 2);
    push_text(&mut doc, &format_date(date), 16, Alignment::Center, false);
    push_br(&mut doc, 2);
    push_text(&mut doc, title, 12, Alignment::Center, false);
    push_br(&mut doc, 8);
    push_text(&mut doc, BRAND_URL, 9, Alignment::Center, false);
    // Force page break after cover (genpdf has no native PageBreak; fill with blank lines)
    push_br(&mut doc, 35);

    // ── Body ──
    let photos = load_photos(app_dir, date)?;
    let thumb_dir = app_dir.join("thumbnails");

    // Logo in top-right of body page
    push_text(&mut doc, BRAND, 10, Alignment::Right, true);
    push_br(&mut doc, 2);

    // Photos (chronological)
    if !photos.is_empty() {
        push_text(&mut doc, "Photos", 12, Alignment::Left, true);
        push_br(&mut doc, 1);
        for photo in &photos {
            let thumb_path = thumb_dir.join(format!("{}.jpg", photo.file_path_hash));
            if thumb_path.exists() {
                if let Ok(img) = elements::Image::from_path(&thumb_path) {
                    doc.push(img);
                    let cap = build_caption(photo);
                    if !cap.is_empty() {
                        push_text(&mut doc, &cap, 8, Alignment::Center, false);
                    }
                    push_br(&mut doc, 2);
                }
            }
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
