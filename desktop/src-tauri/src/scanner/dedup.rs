//! Photo deduplication: content hash + temporal burst clustering.
//!
//! Layer 1: SHA-256 of first 4 KB → catches same file in different folders.
//! Layer 2: Time clustering (gap ≤ 10 s) + self-contained aHash → catches burst shots.

use crate::types::PhotoMeta;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub(crate) const BURST_GAP_SECS: i64 = 30;
pub(crate) const AHASH_HAMMING_THRESHOLD: u32 = 5;
const MAX_BURST_HASH: usize = 20; // skip aHash for giant groups (rare edge case)

/// Run both dedup layers on a list of photos.
pub fn dedup(photos: Vec<PhotoMeta>) -> Vec<PhotoMeta> {
    let photos = dedup_content(photos);
    dedup_burst(photos, BURST_GAP_SECS)
}

// ── Layer 1: content-identical files ──

fn dedup_content(photos: Vec<PhotoMeta>) -> Vec<PhotoMeta> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut kept: Vec<PhotoMeta> = Vec::new();

    for p in photos {
        let sig = content_signature(&p.file_path);
        if let Some(hash) = sig {
            if let Some(&idx) = seen.get(&hash) {
                let existing_size = file_size(&kept[idx].file_path);
                let new_size = file_size(&p.file_path);
                if new_size > existing_size {
                    kept[idx] = p;
                }
                continue;
            } else {
                seen.insert(hash, kept.len());
            }
        }
        kept.push(p);
    }
    kept
}

fn content_signature(path: &str) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 4096];
    let n = f.read(&mut buf).ok()?;
    let len = std::fs::metadata(path).ok()?.len();
    let mut hasher = Sha256::new();
    hasher.update(&buf[..n]);
    hasher.update(len.to_le_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

fn file_size(path: &str) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

// ── Layer 2: burst shot clustering with self-contained aHash ──
// aHash (Average Hash): resize to 8×8 grayscale, compare each pixel to mean.
// Produces a 64-bit fingerprint. Hamming distance ≤ 5 → same scene.

fn dedup_burst(photos: Vec<PhotoMeta>, max_gap_secs: i64) -> Vec<PhotoMeta> {
    let mut with_ts: Vec<(PhotoMeta, Option<i64>)> = photos
        .into_iter()
        .map(|p| {
            let ts = parse_timestamp(&p.timestamp);
            (p, ts)
        })
        .collect();

    with_ts.sort_by_key(|(_, ts)| ts.unwrap_or(i64::MAX));

    // Split into burst groups (gap ≤ max_gap_secs)
    let mut groups: Vec<Vec<(PhotoMeta, Option<i64>)>> = Vec::new();
    for (p, ts) in with_ts {
        if let (Some(prev_group), Some(cur_ts)) = (groups.last_mut(), ts) {
            if let Some(prev_ts) = prev_group.last().and_then(|(_, t)| *t) {
                if cur_ts - prev_ts <= max_gap_secs {
                    prev_group.push((p, Some(cur_ts)));
                    continue;
                }
            }
        }
        groups.push(vec![(p, ts)]);
    }

    let mut result = Vec::new();
    for group in groups {
        if group.len() <= 1 {
            result.extend(group.into_iter().map(|(p, _)| p));
        } else {
            result.extend(dedup_group_by_ahash(group));
        }
    }

    result.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    result
}

fn dedup_group_by_ahash(group: Vec<(PhotoMeta, Option<i64>)>) -> Vec<PhotoMeta> {
    let hashed: Vec<(PhotoMeta, Option<u64>)> = group
        .into_iter()
        .map(|(p, _)| {
            let h = compute_ahash(&p.file_path);
            (p, h)
        })
        .collect();

    let mut kept: Vec<bool> = vec![true; hashed.len()];
    for i in 0..hashed.len() {
        if !kept[i] {
            continue;
        }
        for j in (i + 1)..hashed.len() {
            if !kept[j] {
                continue;
            }
            if let (Some(hi), Some(hj)) = (hashed[i].1, hashed[j].1) {
                if hamming_distance(hi, hj) <= AHASH_HAMMING_THRESHOLD {
                    let si = file_size(&hashed[i].0.file_path);
                    let sj = file_size(&hashed[j].0.file_path);
                    if sj > si {
                        kept[i] = false;
                    } else {
                        kept[j] = false;
                    }
                }
            }
        }
    }

    hashed
        .into_iter()
        .enumerate()
        .filter(|(i, _)| kept[*i])
        .map(|(_, (p, _))| p)
        .collect()
}

/// Self-contained aHash: resize to 8×8 grayscale, compare each pixel to mean.
pub(crate) fn compute_ahash(path: &str) -> Option<u64> {
    let img = image::open(path).ok()?;
    // thumbnail_exact uses Nearest neighbor — 10×+ faster than Lanczos3, same quality at 8×8
    let thumb = img.thumbnail_exact(8, 8);
    let gray = thumb.to_luma8();

    // Compute mean
    let total: u32 = gray.iter().map(|&b| b as u32).sum();
    let mean = (total / 64) as u8;

    // Build 64-bit hash: bit = 1 if pixel > mean
    let mut hash: u64 = 0;
    for (i, &pixel) in gray.iter().enumerate() {
        if pixel > mean {
            hash |= 1u64 << i;
        }
    }
    Some(hash)
}

/// Hamming distance between two 64-bit hashes.
pub(crate) fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

fn parse_timestamp(s: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}
