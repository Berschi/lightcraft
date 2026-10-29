//! Media Organizer workflow commands:
//! - Source (Todo) → Target folder mappings and recursive subfolder scanning
//! - Per-photo staged destination subfolder (`organizer.stageSubfolder`) and batch move (`organizer.moveImages`)
//! - Screenshot filename pattern detection (`organizer.detectScreenshots`) and conversion (`organizer.convertScreenshots`)
//! - PiGallery2 URL generation (`organizer.pigalleryUrl`) and MyStuff MongoDB integration (`organizer.mystuffLoadGames`, `organizer.mystuffSaveMedia`)

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use lightcraft_catalog::{PhotoId, Source};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, bool_or, cmd, has_selection, str_param};
use crate::rename::{RealFs, RenamePlan, folder_template_error};
use crate::{EngineError, LibrarySource, Result, Session};

/// A mapping from a source ("todo") folder to a target root directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderMapping {
    pub source: String,
    pub target_root: String,
}

/// Persistent + session state for the Media Organizer workflow.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OrganizerState {
    /// Configured source → target root mappings (pre-populated with user defaults).
    pub mappings: Vec<FolderMapping>,
    /// Currently selected target folder (e.g. `/home/berschi/syncthing/Pictures/Fotos/Fotos/2026`).
    pub target_folder: Option<String>,
    /// Whether staged subfolder badges are drawn on thumbnails and fullscreen view.
    pub show_subfolders_on_images: bool,
    /// Whether all tags of each photo are rendered as pills in a bar beneath the badge bar in the grid.
    #[serde(default)]
    pub show_tags_in_grid: bool,
    /// Whether photo files and their XMP sidecars are moved to the system trash on delete.
    #[serde(default)]
    pub trash_on_delete: bool,
    /// Whether rating stars are colored by rating level (3 = yellow, 4 = orange, 5 = red).
    #[serde(default)]
    pub colored_rating_stars: bool,
    /// Whether rating stars are shown in the top-left corner in fullscreen view.
    #[serde(default)]
    pub show_rating_in_fullscreen: bool,
    /// Whether only letterboxed photos (with horizontal black bars top and bottom) are shown.
    #[serde(default)]
    pub filter_letterbox: bool,
    /// Cached letterbox detection result per photo.
    #[serde(skip)]
    pub letterbox_cache: BTreeMap<PhotoId, bool>,
    /// Recently chosen destination subfolders (most recent first).
    pub recent_subfolders: Vec<String>,
    /// Base URL for PiGallery2 (e.g. `https://shirogane.fritz.box/pigallery2`).
    pub pigallery_base_url: String,
    /// Preferred MongoDB host for MyStuff (`shirogane` or `127.0.0.1`).
    pub mystuff_host: String,
    /// Staged relative destination subfolder per photo (session-only; cleared when moved).
    #[serde(skip)]
    pub staged_moves: BTreeMap<PhotoId, String>,
    /// Cached games for tests or offline use (keyed by host).
    #[serde(skip)]
    pub mock_games: BTreeMap<String, Vec<MyStuffGameEntry>>,
    /// Cached `(target_folder, subfolders)` so UI dialogs never re-scan the filesystem per frame.
    #[serde(skip)]
    pub cached_subfolders: Option<(String, Vec<String>)>,
    /// Cached `root -> target_folders` so dropdowns and `sync_target_folder_for_source` don't re-scan per frame.
    #[serde(skip)]
    pub cached_target_folders: BTreeMap<String, Vec<String>>,
    /// Last source folder for which `sync_target_folder_for_source` ran.
    #[serde(skip)]
    pub last_synced_source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyStuffMediaEntry {
    pub name: String,
    pub location: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyStuffGameEntry {
    pub id: String,
    pub name: String,
    pub media: Vec<MyStuffMediaEntry>,
}

pub fn default_mappings() -> Vec<FolderMapping> {
    vec![
        FolderMapping {
            source: "/home/berschi/syncthing/Camera Upload/_todo_media".into(),
            target_root: "/home/berschi/syncthing/Pictures/Media".into(),
        },
        FolderMapping {
            source: "/home/berschi/syncthing/Camera Upload/_todo_photos".into(),
            target_root: "/home/berschi/syncthing/Pictures/Fotos/Fotos".into(),
        },
        FolderMapping { source: "/home/berschi/syncthing/Camera Upload/_videos_to_sort".into(), target_root: "/mnt/HDD/syncthing_HDD/Videos".into() },
    ]
}

impl Default for OrganizerState {
    fn default() -> Self {
        Self {
            mappings: default_mappings(),
            target_folder: None,
            show_subfolders_on_images: true,
            show_tags_in_grid: false,
            trash_on_delete: false,
            colored_rating_stars: false,
            show_rating_in_fullscreen: false,
            filter_letterbox: false,
            letterbox_cache: BTreeMap::new(),
            recent_subfolders: Vec::new(),
            pigallery_base_url: "https://shirogane.fritz.box/pigallery2".into(),
            mystuff_host: "shirogane".into(),
            staged_moves: BTreeMap::new(),
            mock_games: BTreeMap::new(),
            cached_subfolders: None,
            cached_target_folders: BTreeMap::new(),
            last_synced_source: None,
        }
    }
}

impl OrganizerState {
    pub fn sanitized(mut self) -> Self {
        if self.mappings.is_empty() {
            self.mappings = default_mappings();
        }
        if self.pigallery_base_url.trim().is_empty() {
            self.pigallery_base_url = "https://shirogane.fritz.box/pigallery2".into();
        }
        if self.mystuff_host.trim().is_empty() {
            self.mystuff_host = "shirogane".into();
        }
        self
    }

    /// Return cached subfolders for `target_folder`, scanning disk only on cache miss.
    pub fn subfolders_for(&mut self, target_folder: &str) -> &[String] {
        let tf = trim_slashes(target_folder);
        if tf.is_empty() {
            return &[];
        }
        let needs_load = self.cached_subfolders.as_ref().is_none_or(|(cached_tf, _)| cached_tf != &tf);
        if needs_load {
            let subs = list_subfolders(&tf);
            self.cached_subfolders = Some((tf, subs));
        }
        self.cached_subfolders.as_ref().map(|(_, v)| v.as_slice()).unwrap_or(&[])
    }

    /// Return cached child target folders for `root`, scanning disk only on cache miss.
    pub fn target_folders_for(&mut self, root: &str) -> Vec<String> {
        let r = trim_slashes(root);
        if r.is_empty() {
            return Vec::new();
        }
        if let Some(cached) = self.cached_target_folders.get(&r) {
            return cached.clone();
        }
        let list = list_target_folders(&r);
        self.cached_target_folders.insert(r, list.clone());
        list
    }

    /// Clear cached folder listings (called when folders are created or mappings change).
    pub fn invalidate_folder_cache(&mut self) {
        self.cached_subfolders = None;
        self.cached_target_folders.clear();
        self.last_synced_source = None;
        self.letterbox_cache.clear();
    }

    /// Record a newly created relative subfolder inside `target_folder` without re-scanning disk if cached.
    pub fn note_subfolder_created(&mut self, target_folder: &str, sub: &str) {
        let tf = trim_slashes(target_folder);
        if let Some((cached_tf, list)) = &mut self.cached_subfolders
            && cached_tf == &tf
        {
            let mut acc = String::new();
            for part in sub.split('/').filter(|p| !p.is_empty() && *p != ".") {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                if let Err(pos) = list.binary_search(&acc) {
                    list.insert(pos, acc.clone());
                }
            }
        }
    }
}

fn trim_slashes(p: &str) -> String {
    p.trim().trim_end_matches(['/', '\\']).to_string()
}

/// Fast directory check using `DirEntry::file_type()` (`d_type` on Linux) so regular files do not trigger `stat()` syscalls.
fn is_entry_dir(e: &std::fs::DirEntry) -> bool {
    match e.file_type() {
        Ok(ft) if ft.is_dir() => true,
        Ok(ft) if ft.is_symlink() => e.path().is_dir(),
        _ => false,
    }
}

/// Active source folder path (either `LibrarySource::Folder` browse path, `LibrarySource::LibraryFolder`,
/// or the parent folder of the active photo).
pub fn current_source_folder(s: &Session) -> Option<String> {
    match s.source {
        LibrarySource::Folder => s.browse.as_ref().map(|b| trim_slashes(&b.path)),
        LibrarySource::LibraryFolder => s.library_folder.as_deref().map(trim_slashes),
        _ => s.active().and_then(|id| s.catalog.photo(id)).and_then(|p| match &p.source {
            Source::File { path } => Path::new(path).parent().map(|d| trim_slashes(&d.to_string_lossy())),
            Source::Demo { .. } => None,
        }),
    }
}

/// Find the mapped `target_root` for `source_folder` if it matches a configured mapping
/// (either exact prefix or matching todo folder basename like `_todo_photos`, `_todo_media`, `_videos_to_sort`).
pub fn matched_target_root(s: &Session, source_folder: Option<&str>) -> Option<String> {
    let src = trim_slashes(source_folder?);
    if src.is_empty() {
        return None;
    }
    for m in &s.organizer.mappings {
        let ms = trim_slashes(&m.source);
        if !ms.is_empty() && (src == ms || src.starts_with(&format!("{ms}/"))) {
            return Some(trim_slashes(&m.target_root));
        }
    }
    let src_name = Path::new(&src).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if !src_name.is_empty() {
        for m in &s.organizer.mappings {
            let m_name = Path::new(m.source.trim_end_matches(['/', '\\'])).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if m_name == src_name {
                return Some(trim_slashes(&m.target_root));
            }
        }
    }
    None
}

/// Immediate child folders of `root` (e.g. `2026`, `2025` under `Fotos/Fotos`), plus `root` itself.
pub fn list_target_folders(root: &str) -> Vec<String> {
    let root = trim_slashes(root);
    if root.is_empty() {
        return Vec::new();
    }
    let mut out = vec![root.clone()];
    let p = Path::new(&root);
    if let Ok(entries) = std::fs::read_dir(p) {
        let mut children: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                if !is_entry_dir(&e) {
                    return None;
                }
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') && name != "thumb_cache" { Some(trim_slashes(&e.path().to_string_lossy())) } else { None }
            })
            .collect();
        children.sort_by(|a, b| b.cmp(a));
        out.extend(children);
    }
    out
}

/// List all relative subfolders inside `target_folder` (bounded depth/count so a huge tree never blocks).
pub fn list_subfolders(target_folder: &str) -> Vec<String> {
    let base = Path::new(target_folder.trim());
    if !base.is_dir() {
        return Vec::new();
    }
    let mut out = BTreeSet::new();
    out.insert(".".to_string());
    collect_subfolders(base, base, 0, &mut out);
    out.into_iter().collect()
}

fn collect_subfolders(base: &Path, current: &Path, depth: usize, out: &mut BTreeSet<String>) {
    if depth >= 5 || out.len() >= 2500 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else { return };
    for e in entries.flatten() {
        if out.len() >= 2500 {
            break;
        }
        if !is_entry_dir(&e) {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "thumb_cache" {
            continue;
        }
        let path = e.path();
        if let Ok(rel) = path.strip_prefix(base) {
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            if !rel_s.is_empty() {
                out.insert(rel_s);
                collect_subfolders(base, &path, depth + 1, out);
            }
        }
    }
}

/// Automatically select a sensible default `target_folder` when entering a mapped source folder.
pub fn sync_target_folder_for_source(s: &mut Session) {
    let src = current_source_folder(s);
    if s.organizer.target_folder.is_some() && src == s.organizer.last_synced_source {
        return;
    }
    s.organizer.last_synced_source = src.clone();
    if let Some(root) = matched_target_root(s, src.as_deref()) {
        let keep_current = s.organizer.target_folder.as_ref().is_some_and(|tf| {
            let tf = trim_slashes(tf);
            tf == root || tf.starts_with(&format!("{root}/"))
        });
        if !keep_current {
            let candidates = s.organizer.target_folders_for(&root);
            // If `root` is `Fotos/Fotos` and has year folders like `2026`, pick the newest year folder if it starts with `20`;
            // otherwise default to `root` itself.
            let chosen = candidates
                .iter()
                .skip(1)
                .find(|c| {
                    Path::new(c).file_name().map(|n| n.to_string_lossy()).is_some_and(|n| n.len() == 4 && n.chars().all(|ch| ch.is_ascii_digit()))
                })
                .cloned()
                .unwrap_or(root);
            s.organizer.target_folder = Some(chosen);
        }
    } else if s.organizer.target_folder.is_none()
        && let Some(src) = src
    {
        s.organizer.target_folder = Some(src);
    }
}

/// Validate a relative destination subfolder (`"."` = target folder root itself).
pub fn validate_subfolder(sub: &str) -> std::result::Result<String, String> {
    let trimmed = sub.trim().replace('\\', "/");
    let trimmed = trimmed.trim_matches('/').trim().to_string();
    if trimmed.is_empty() || trimmed == "." {
        return Ok(".".into());
    }
    if let Some(err) = folder_template_error(&trimmed) {
        return Err(err);
    }
    Ok(trimmed)
}

/// Check if a folder exists on disk and is completely empty (ignoring system metadata like `.DS_Store`, `Thumbs.db`, `.directory`).
pub fn is_folder_empty(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return false };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".DS_Store" || name_str == "Thumbs.db" || name_str == ".directory" {
            continue;
        }
        return false;
    }
    true
}

/// Check if a folder path is safe to delete (not root `/`, drive root, or user's `$HOME`).
pub fn is_safe_to_delete_folder(path: &Path) -> bool {
    let Ok(abs) = std::path::absolute(path) else { return false };
    let Some(parent) = abs.parent() else { return false };
    if parent == Path::new("") || parent == Path::new("/") {
        return false;
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && abs == Path::new(&home)
    {
        return false;
    }
    true
}

/// URL-encode a path segment or relative path for PiGallery2 (encoding everything except unreserved RFC 3986 chars).
fn url_encode_all(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.as_bytes() {
        let c = *b;
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(c));
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// Compute the PiGallery2 URL for `folder_path` (matching `pigallery2_helper.py`).
pub fn pigallery_url_for_path(s: &Session, folder_path: Option<&str>) -> Option<String> {
    let folder = trim_slashes(folder_path?);
    if folder.is_empty() {
        return None;
    }
    let base_url = trim_slashes(&s.organizer.pigallery_base_url);
    let candidate = PathBuf::from(&folder);

    // 1. Check configured target roots first (lexical fast-path first, canonicalize fallback only if needed)
    for m in &s.organizer.mappings {
        let root_path = PathBuf::from(trim_slashes(&m.target_root));
        let root_name = root_path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let category = match root_name.as_str() {
            "media" => Some("media"),
            "photos" | "fotos" => Some("photos"),
            "videos" => Some("videos"),
            _ => None,
        };
        let Some(cat) = category else { continue };
        let rel_opt = candidate.strip_prefix(&root_path).map(|p| p.to_path_buf()).ok().or_else(|| {
            let can_cand = std::fs::canonicalize(&candidate).ok()?;
            let can_root = std::fs::canonicalize(&root_path).ok()?;
            can_cand.strip_prefix(&can_root).ok().map(|p| p.to_path_buf())
        });
        if let Some(rel) = rel_opt {
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            let rel_s = rel_s.trim_matches('/');
            let cat_url = format!("{base_url}/{cat}/gallery");
            return Some(if rel_s.is_empty() || rel_s == "." { cat_url } else { format!("{cat_url}/{}", url_encode_all(rel_s)) });
        }
    }

    // 2. Fallback: check if any ancestor directory is named Media, Fotos, Photos, or Videos
    let components: Vec<String> = candidate.iter().map(|c| c.to_string_lossy().to_string()).collect();
    for (idx, comp) in components.iter().enumerate().rev() {
        let lower = comp.to_lowercase();
        let cat = match lower.as_str() {
            "media" => Some("media"),
            "photos" | "fotos" => Some("photos"),
            "videos" => Some("videos"),
            _ => None,
        };
        if let Some(cat) = cat {
            let rel_parts = components.get(idx + 1..).unwrap_or(&[]);
            let rel_s = rel_parts.join("/");
            let cat_url = format!("{base_url}/{cat}/gallery");
            return Some(if rel_s.is_empty() { cat_url } else { format!("{cat_url}/{}", url_encode_all(&rel_s)) });
        }
    }
    None
}

fn state_json(s: &mut Session) -> Value {
    if s.organizer.target_folder.is_none() {
        sync_target_folder_for_source(s);
    }
    let src = current_source_folder(s);
    let matched_root = matched_target_root(s, src.as_deref());
    let target_roots: Vec<String> = s.organizer.mappings.iter().map(|m| trim_slashes(&m.target_root)).collect();
    let active_root = matched_root.clone().or_else(|| s.organizer.target_folder.clone()).or_else(|| target_roots.first().cloned());
    let target_folders = active_root.as_deref().map(|r| s.organizer.target_folders_for(r)).unwrap_or_default();
    let tf_clone = s.organizer.target_folder.clone();
    let subfolders = tf_clone.as_deref().map(|tf| s.organizer.subfolders_for(tf).to_vec()).unwrap_or_default();
    let staged: BTreeMap<String, String> = s
        .organizer
        .staged_moves
        .iter()
        .filter(|(id, _)| s.catalog.photo(**id).is_some_and(|p| !p.deleted))
        .map(|(id, sub)| (id.0.to_string(), sub.clone()))
        .collect();
    let pigallery_folder = s.organizer.target_folder.as_deref().or(src.as_deref());
    let pigallery_url = pigallery_url_for_path(s, pigallery_folder);
    json!({
        "sourceFolder": src,
        "matchedTargetRoot": matched_root,
        "targetRoots": target_roots,
        "targetFolders": target_folders,
        "targetFolder": s.organizer.target_folder,
        "subfolders": subfolders,
        "recentSubfolders": s.organizer.recent_subfolders,
        "stagedMoves": staged,
        "stagedCount": staged.len(),
        "showSubfoldersOnImages": s.organizer.show_subfolders_on_images,
        "showTagsInGrid": s.organizer.show_tags_in_grid,
        "trashOnDelete": s.organizer.trash_on_delete,
        "coloredRatingStars": s.organizer.colored_rating_stars,
        "showRatingInFullscreen": s.organizer.show_rating_in_fullscreen,
        "filterLetterbox": s.organizer.filter_letterbox,
        "pigalleryUrl": pigallery_url,
        "mappings": s.organizer.mappings,
        "mystuffHost": s.organizer.mystuff_host,
    })
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, ext),
        _ => (name, ""),
    }
}

fn remember_recent_subfolder(s: &mut Session, sub: &str) {
    if sub.is_empty() || sub == "." {
        return;
    }
    s.organizer.recent_subfolders.retain(|x| !x.eq_ignore_ascii_case(sub));
    s.organizer.recent_subfolders.insert(0, sub.to_string());
    if s.organizer.recent_subfolders.len() > 25 {
        s.organizer.recent_subfolders.truncate(25);
    }
}

/// Move all staged photos (or the explicitly passed `ids` if staged/subfolder given) into `<targetFolder>/<subfolder>`.
fn move_images(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "organizer.moveImages";
    // The sidecars written above (and the files' own XMP written below) replace automatic writing.
    s.skip_auto_write = true;
    if s.organizer.target_folder.is_none() {
        sync_target_folder_for_source(s);
    }
    let target_base = str_param(p, "targetFolder")
        .map(trim_slashes)
        .or_else(|| s.organizer.target_folder.as_deref().map(trim_slashes))
        .filter(|t| !t.is_empty())
        .ok_or_else(|| bad(C, "no target folder selected (`organizer.setTargetFolder` first)"))?;

    let direct_sub = match str_param(p, "subfolder") {
        Some(sub) => Some(validate_subfolder(sub).map_err(|e| bad(C, e))?),
        None => None,
    };

    // Determine which photos to move and their destination subfolders.
    let mut pairs: Vec<(PhotoId, String)> = Vec::new();
    if let Some(sub) = direct_sub {
        let ids = s.targets(p)?;
        for id in ids {
            pairs.push((id, sub.clone()));
        }
    } else if super::names_photos(p) {
        for id in s.targets(p)? {
            if let Some(sub) = s.organizer.staged_moves.get(&id) {
                pairs.push((id, sub.clone()));
            }
        }
    } else {
        // Move all staged photos in visible order first, then any remaining staged photos.
        let vis = s.visible_cloned();
        let mut seen = HashSet::new();
        for id in vis {
            if let Some(sub) = s.organizer.staged_moves.get(&id) {
                seen.insert(id);
                pairs.push((id, sub.clone()));
            }
        }
        for (id, sub) in &s.organizer.staged_moves {
            if !seen.contains(id) && s.catalog.photo(*id).is_some_and(|ph| !ph.deleted) {
                pairs.push((*id, sub.clone()));
            }
        }
    }

    if pairs.is_empty() {
        return Err(bad(C, "no photos have a destination subfolder assigned (press M to assign a subfolder first)"));
    }

    // Write each RAW or other non-JPEG/PNG file photo's XMP sidecar before it moves, so the sidecar
    // that travels with it holds its current tags, crops and develop settings even when automatic
    // XMP writing is off. A failure is reported but does not stop the move. JPEG and PNG photos
    // get their data written into the file after the move instead (see below).
    let ids_to_move: Vec<PhotoId> = pairs.iter().map(|(id, _)| *id).collect();
    let owners = crate::sidecar::StemOwners::among(&s.catalog, &ids_to_move);
    let mut sidecars_failed = Vec::new();
    for id in &ids_to_move {
        let is_sidecar_original = s
            .catalog
            .photo(*id)
            .is_some_and(|ph| ph.copy_of.is_none() && matches!(&ph.source, Source::File { path } if !crate::bake::is_bakeable(path)));
        if is_sidecar_original && let Err(e) = s.save_sidecar_with(*id, &owners) {
            sidecars_failed.push(json!({"id": id.0, "error": e.to_string()}));
        }
    }

    // Build RenamePlans, creating subfolders on disk and resolving collisions with `_1`, `_2`...
    let mut taken: HashSet<String> = HashSet::new();
    let mut plans: Vec<RenamePlan> = Vec::new();
    let mut moved_ids: Vec<PhotoId> = Vec::new();
    let mut seen_src_paths: HashSet<String> = HashSet::new();

    for (id, sub) in &pairs {
        let Some(ph) = s.catalog.photo(*id) else { continue };
        let Source::File { path: src_path } = &ph.source else { continue };
        if !seen_src_paths.insert(src_path.clone()) {
            moved_ids.push(*id);
            continue;
        }
        let dest_dir = if sub == "." || sub.is_empty() { PathBuf::from(&target_base) } else { PathBuf::from(&target_base).join(sub) };
        std::fs::create_dir_all(&dest_dir).map_err(|e| EngineError::Other(format!("create {}: {e}", dest_dir.display())))?;

        let (stem, ext) = split_ext(&ph.file_name);
        let mut k = 0usize;
        let (to_name, to_path) = loop {
            let candidate_name = if k == 0 {
                ph.file_name.clone()
            } else if ext.is_empty() {
                format!("{stem}_{k}")
            } else {
                format!("{stem}_{k}.{ext}")
            };
            let cand_path = dest_dir.join(&candidate_name);
            let cand_str = cand_path.to_string_lossy().to_string();
            let key = cand_str.to_lowercase();
            let same = Path::new(src_path) == cand_path.as_path();
            if !taken.contains(&key) && (same || !cand_path.exists()) {
                taken.insert(key);
                break (candidate_name, cand_str);
            }
            k = k.saturating_add(1);
        };

        moved_ids.push(*id);
        plans.push(RenamePlan { id: id.0, from: ph.file_name.clone(), to: to_name, from_path: Some(src_path.clone()), to_path: Some(to_path) });
    }

    let moved_count = s.apply_file_plans(&RealFs, &plans, &|n| format!("Move {n} Photo{}", if n == 1 { "" } else { "s" }))?;

    // Each JPEG or PNG now sits at its new path: write its tags and edits into it. One failing file
    // doesn't stop the others; its photo stays moved, and the error is reported.
    let mut image_failed = Vec::new();
    let mut images_written = 0usize;
    for plan in &plans {
        let Some(to) = plan.to_path.as_deref() else { continue };
        if !crate::bake::is_bakeable(to) {
            continue;
        }
        match crate::bake::write_into_file(s, PhotoId(plan.id), Path::new(to)) {
            Ok(()) => images_written += 1,
            Err(error) => image_failed.push(json!({"id": plan.id, "error": error})),
        }
    }

    for (_, sub) in &pairs {
        remember_recent_subfolder(s, sub);
        s.organizer.note_subfolder_created(&target_base, sub);
    }
    for id in &moved_ids {
        s.organizer.staged_moves.remove(id);
    }
    s.keep_active_in_view();
    let _ = s.save_prefs();

    Ok(json!({
        "moved": moved_count,
        "plans": plans,
        "remainingStaged": s.organizer.staged_moves.len(),
        "sidecarsFailed": sidecars_failed,
        "imagesWritten": images_written,
        "imageFailed": image_failed,
    }))
}

// ---------- Screenshot filename pattern detection & conversion ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenshotPattern {
    SharexSpectacle,
    Nvidia,
    Amd,
    Steam,
    Camera,
    SteamOld,
    SteamOtherOld,
    Switch,
    Playstation,
    WindowsRecord,
    DownloadRename,
}

impl ScreenshotPattern {
    pub const ALL: &'static [ScreenshotPattern] = &[
        Self::SharexSpectacle,
        Self::Nvidia,
        Self::Amd,
        Self::Steam,
        Self::Camera,
        Self::SteamOld,
        Self::SteamOtherOld,
        Self::Switch,
        Self::Playstation,
        Self::WindowsRecord,
        Self::DownloadRename,
    ];

    pub const CONVERTIBLE: &'static [ScreenshotPattern] = &[Self::SharexSpectacle, Self::Nvidia, Self::Amd, Self::Steam, Self::Camera];

    pub fn id(self) -> &'static str {
        match self {
            Self::SharexSpectacle => "SHAREX_SPECTACLE",
            Self::Nvidia => "NVIDIA",
            Self::Amd => "AMD",
            Self::Steam => "STEAM",
            Self::Camera => "CAMERA",
            Self::SteamOld => "STEAM_OLD",
            Self::SteamOtherOld => "STEAM_OTHER_OLD",
            Self::Switch => "SWITCH",
            Self::Playstation => "PLAYSTATION",
            Self::WindowsRecord => "WINDOWS_RECORD",
            Self::DownloadRename => "DOWNLOAD_RENAME",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SharexSpectacle => "ShareX / Spectacle (YYYY-MM-DD_HH.MM.SS.mmm)",
            Self::Nvidia => "NVIDIA (YYYY.MM.DD - HH.MM.SS.00)",
            Self::Amd => "AMD (YYYY.MM.DD-HH.MM)",
            Self::Steam => "Steam (YYYYMMDDHHMMSS_1)",
            Self::Camera => "Camera (YYYYMMDD_HHMMSS)",
            Self::SteamOld => "Steam Old (YYYY-MM-DD_NNNNN)",
            Self::SteamOtherOld => "Steam Legacy (YYYY-MM-DD__HHMMSS_1)",
            Self::Switch => "Nintendo Switch (16 digits)",
            Self::Playstation => "PlayStation (Game_YYYYMMDDHHMMSS)",
            Self::WindowsRecord => "Windows Capture (YYYY-MM-DD)",
            Self::DownloadRename => "32-char Hash / Download",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        let u = s.trim().to_ascii_uppercase();
        Self::ALL.iter().copied().find(|p| p.id() == u)
    }

    pub fn is_convertible(self) -> bool {
        Self::CONVERTIBLE.contains(&self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParsedTimestamp {
    pub(crate) year: u32,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) hour: u32,
    pub(crate) minute: u32,
    pub(crate) second: u32,
    pub(crate) millis: u32,
}

fn parse_digits(s: &str, range: std::ops::Range<usize>) -> Option<u32> {
    let sub = s.get(range)?;
    if !sub.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    sub.parse::<u32>().ok()
}

fn char_at(s: &str, idx: usize) -> Option<u8> {
    s.as_bytes().get(idx).copied()
}

fn valid_ymd_hms(y: u32, m: u32, d: u32, h: u32, min: u32, sec: u32) -> bool {
    (1970..=2100).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d) && h <= 23 && min <= 59 && sec <= 59
}

fn match_sharex_spectacle(stem: &str) -> Option<ParsedTimestamp> {
    // `\d{4}-\d{2}-\d{2}_\d{2}.\d{2}.\d{2}.\d{3}` (length 23)
    if stem.len() != 23 || char_at(stem, 4) != Some(b'-') || char_at(stem, 7) != Some(b'-') || char_at(stem, 10) != Some(b'_') {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 5..7)?;
    let d = parse_digits(stem, 8..10)?;
    let h = parse_digits(stem, 11..13)?;
    let min = parse_digits(stem, 14..16)?;
    let sec = parse_digits(stem, 17..19)?;
    let ms = parse_digits(stem, 20..23)?;
    valid_ymd_hms(y, m, d, h, min, sec).then_some(ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: sec, millis: ms })
}

fn match_nvidia(stem: &str) -> Option<ParsedTimestamp> {
    // `\d{4}\.\d{2}\.\d{2}\s-\s\d{2}\.\d{2}\.\d{2}(\s-\s\(\d+\))?(.\d{2,3})?`
    if stem.len() < 21
        || char_at(stem, 4) != Some(b'.')
        || char_at(stem, 7) != Some(b'.')
        || stem.get(10..13) != Some(" - ")
        || char_at(stem, 15) != Some(b'.')
        || char_at(stem, 18) != Some(b'.')
    {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 5..7)?;
    let d = parse_digits(stem, 8..10)?;
    let h = parse_digits(stem, 13..15)?;
    let min = parse_digits(stem, 16..18)?;
    let sec = parse_digits(stem, 19..21)?;
    if !valid_ymd_hms(y, m, d, h, min, sec) {
        return None;
    }
    let mut rest = stem.get(21..).unwrap_or("");
    // Optional ` - (\d+)`
    if let Some(after_paren_prefix) = rest.strip_prefix(" - (") {
        let close = after_paren_prefix.find(')')?;
        let digits = after_paren_prefix.get(..close)?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        rest = after_paren_prefix.get(close + 1..).unwrap_or("");
    }
    // Optional `.\d{2,3}`
    let millis = if rest.is_empty() {
        0
    } else {
        let digits = rest.get(1..)?;
        if !(2..=3).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let val = digits.parse::<u32>().ok()?;
        if digits.len() == 2 { val * 10 } else { val }
    };
    Some(ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: sec, millis })
}

fn match_amd(stem: &str) -> Option<ParsedTimestamp> {
    // `\d{4}\.\d{2}\.\d{2}-\d{2}\.\d{2}(_\d{1,2})?`
    if stem.len() < 16
        || char_at(stem, 4) != Some(b'.')
        || char_at(stem, 7) != Some(b'.')
        || char_at(stem, 10) != Some(b'-')
        || char_at(stem, 13) != Some(b'.')
    {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 5..7)?;
    let d = parse_digits(stem, 8..10)?;
    let h = parse_digits(stem, 11..13)?;
    let min = parse_digits(stem, 14..16)?;
    if !valid_ymd_hms(y, m, d, h, min, 0) {
        return None;
    }
    let rest = stem.get(16..).unwrap_or("");
    if !rest.is_empty() {
        let digits = rest.strip_prefix('_')?;
        if !(1..=2).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: 0, millis: 0 })
}

fn match_steam(stem: &str) -> Option<ParsedTimestamp> {
    // `\d{14}_\d` (length 16)
    if stem.len() != 16 || char_at(stem, 14) != Some(b'_') || !char_at(stem, 15).is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 4..6)?;
    let d = parse_digits(stem, 6..8)?;
    let h = parse_digits(stem, 8..10)?;
    let min = parse_digits(stem, 10..12)?;
    let sec = parse_digits(stem, 12..14)?;
    valid_ymd_hms(y, m, d, h, min, sec).then_some(ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: sec, millis: 0 })
}

fn match_camera(stem: &str) -> Option<ParsedTimestamp> {
    // `^(\d{8}_\d{6})([^.]+)?`
    if stem.len() < 15 || char_at(stem, 8) != Some(b'_') {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 4..6)?;
    let d = parse_digits(stem, 6..8)?;
    let h = parse_digits(stem, 9..11)?;
    let min = parse_digits(stem, 11..13)?;
    let sec = parse_digits(stem, 13..15)?;
    valid_ymd_hms(y, m, d, h, min, sec).then_some(ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: sec, millis: 0 })
}

fn match_steam_old(stem: &str) -> Option<ParsedTimestamp> {
    // `\d{4}-\d{2}-\d{2}_\d{5}(\s\d)?`
    if stem.len() < 16 || char_at(stem, 4) != Some(b'-') || char_at(stem, 7) != Some(b'-') || char_at(stem, 10) != Some(b'_') {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 5..7)?;
    let d = parse_digits(stem, 8..10)?;
    let _seq = parse_digits(stem, 11..16)?;
    let rest = stem.get(16..).unwrap_or("");
    if !rest.is_empty() {
        let d2 = rest.strip_prefix(' ')?;
        if d2.len() != 1 || !d2.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    valid_ymd_hms(y, m, d, 0, 0, 0).then_some(ParsedTimestamp { year: y, month: m, day: d, hour: 0, minute: 0, second: 0, millis: 0 })
}

fn match_steam_other_old(stem: &str) -> Option<ParsedTimestamp> {
    // `\d{4}-\d{2}-\d{2}__\d{6}_\d` (length 20)
    if stem.len() != 20
        || char_at(stem, 4) != Some(b'-')
        || char_at(stem, 7) != Some(b'-')
        || stem.get(10..12) != Some("__")
        || char_at(stem, 18) != Some(b'_')
        || !char_at(stem, 19).is_some_and(|b| b.is_ascii_digit())
    {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 5..7)?;
    let d = parse_digits(stem, 8..10)?;
    let h = parse_digits(stem, 12..14)?;
    let min = parse_digits(stem, 14..16)?;
    let sec = parse_digits(stem, 16..18)?;
    valid_ymd_hms(y, m, d, h, min, sec).then_some(ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: sec, millis: 0 })
}

fn match_switch(stem: &str) -> Option<ParsedTimestamp> {
    // `\d{16}`
    if stem.len() != 16 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 4..6)?;
    let d = parse_digits(stem, 6..8)?;
    let h = parse_digits(stem, 8..10)?;
    let min = parse_digits(stem, 10..12)?;
    let sec = parse_digits(stem, 12..14)?;
    let cs = parse_digits(stem, 14..16)?;
    valid_ymd_hms(y, m, d, h, min, sec).then_some(ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: sec, millis: cs * 10 })
}

fn match_playstation(stem: &str) -> Option<ParsedTimestamp> {
    // `[^_]+_\d{14}(_\d)?`
    let (prefix, rest) = stem.split_once('_')?;
    if prefix.is_empty() || rest.len() < 14 {
        return None;
    }
    let y = parse_digits(rest, 0..4)?;
    let m = parse_digits(rest, 4..6)?;
    let d = parse_digits(rest, 6..8)?;
    let h = parse_digits(rest, 8..10)?;
    let min = parse_digits(rest, 10..12)?;
    let sec = parse_digits(rest, 12..14)?;
    let tail = rest.get(14..).unwrap_or("");
    if !tail.is_empty() {
        let d2 = tail.strip_prefix('_')?;
        if d2.len() != 1 || !d2.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    valid_ymd_hms(y, m, d, h, min, sec).then_some(ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: sec, millis: 0 })
}

fn match_windows_record(stem: &str) -> Option<ParsedTimestamp> {
    // `\d{4}-\d{2}-\d{2}(\s\(\d+\))?`
    if stem.len() < 10 || char_at(stem, 4) != Some(b'-') || char_at(stem, 7) != Some(b'-') {
        return None;
    }
    let y = parse_digits(stem, 0..4)?;
    let m = parse_digits(stem, 5..7)?;
    let d = parse_digits(stem, 8..10)?;
    let tail = stem.get(10..).unwrap_or("");
    if !tail.is_empty() {
        let inner = tail.strip_prefix(" (")?.strip_suffix(')')?;
        if inner.is_empty() || !inner.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    valid_ymd_hms(y, m, d, 0, 0, 0).then_some(ParsedTimestamp { year: y, month: m, day: d, hour: 0, minute: 0, second: 0, millis: 0 })
}

fn match_download_rename(stem: &str) -> Option<ParsedTimestamp> {
    (stem.chars().count() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit() || b.is_ascii_alphanumeric())).then_some(ParsedTimestamp {
        year: 2000,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        millis: 0,
    })
}

/// Match `stem` against `pattern` and extract its timestamp when convertible.
pub(crate) fn match_pattern(stem: &str, pattern: ScreenshotPattern) -> Option<ParsedTimestamp> {
    match pattern {
        ScreenshotPattern::SharexSpectacle => match_sharex_spectacle(stem),
        ScreenshotPattern::Nvidia => match_nvidia(stem),
        ScreenshotPattern::Amd => match_amd(stem),
        ScreenshotPattern::Steam => match_steam(stem),
        ScreenshotPattern::Camera => match_camera(stem),
        ScreenshotPattern::SteamOld => match_steam_old(stem),
        ScreenshotPattern::SteamOtherOld => match_steam_other_old(stem),
        ScreenshotPattern::Switch => match_switch(stem),
        ScreenshotPattern::Playstation => match_playstation(stem),
        ScreenshotPattern::WindowsRecord => match_windows_record(stem),
        ScreenshotPattern::DownloadRename => match_download_rename(stem),
    }
}

pub fn detect_stem_pattern(stem: &str) -> Option<ScreenshotPattern> {
    ScreenshotPattern::ALL.iter().copied().find(|pat| match_pattern(stem, *pat).is_some())
}

/// Extract an ISO timestamp `YYYY-MM-DDTHH:MM:SS.mmm` from a filename stem if it matches
/// a convertible screenshot pattern.
pub fn timestamp_from_stem(stem: &str) -> Option<String> {
    let pat = detect_stem_pattern(stem)?;
    if !pat.is_convertible() {
        return None;
    }
    let ts = match_pattern(stem, pat)?;
    Some(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}", ts.year, ts.month, ts.day, ts.hour, ts.minute, ts.second, ts.millis))
}

fn format_screenshot_stem(ts: ParsedTimestamp, to: ScreenshotPattern, collision_index: usize) -> String {
    let ParsedTimestamp { year: y, month: m, day: d, hour: h, minute: min, second: s, millis: ms } = ts;
    match to {
        ScreenshotPattern::SharexSpectacle => {
            if collision_index == 0 {
                format!("{y:04}-{m:02}-{d:02}_{h:02}.{min:02}.{s:02}.{ms:03}")
            } else {
                format!("{y:04}-{m:02}-{d:02}_{h:02}.{min:02}.{s:02}.{collision_index:03}")
            }
        }
        ScreenshotPattern::Nvidia => {
            if collision_index == 0 {
                format!("{y:04}.{m:02}.{d:02} - {h:02}.{min:02}.{s:02}.00")
            } else {
                format!("{y:04}.{m:02}.{d:02} - {h:02}.{min:02}.{s:02} - ({collision_index}).00")
            }
        }
        ScreenshotPattern::Amd => {
            if collision_index == 0 {
                format!("{y:04}.{m:02}.{d:02}-{h:02}.{min:02}")
            } else {
                format!("{y:04}.{m:02}.{d:02}-{h:02}.{min:02}_{collision_index}")
            }
        }
        ScreenshotPattern::Steam => {
            let seq = collision_index.saturating_add(1);
            format!("{y:04}{m:02}{d:02}{h:02}{min:02}{s:02}_{seq}")
        }
        ScreenshotPattern::Camera => {
            if collision_index == 0 {
                format!("{y:04}{m:02}{d:02}_{h:02}{min:02}{s:02}")
            } else {
                format!("{y:04}{m:02}{d:02}_{h:02}{min:02}{s:02}_{collision_index}")
            }
        }
        _ => format!("{y:04}-{m:02}-{d:02}_{h:02}.{min:02}.{s:02}"),
    }
}

fn target_photos_for_screenshots(s: &mut Session, p: &Value) -> Result<Vec<PhotoId>> {
    if super::names_photos(p) {
        return s.targets(p);
    }
    let all_visible = bool_or(p, "allVisible", s.selection.ids.len() <= 1);
    if all_visible { Ok(s.visible_cloned()) } else { s.targets(p) }
}

fn detect_screenshots(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = target_photos_for_screenshots(s, p)?;
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut unknown = 0usize;
    let mut files = Vec::new();

    for id in ids {
        let Some(ph) = s.catalog.photo(id) else { continue };
        let (stem, _) = split_ext(&ph.file_name);
        match detect_stem_pattern(stem) {
            Some(pat) => {
                *counts.entry(pat.id()).or_insert(0) += 1;
                files.push(json!({"id": id.0, "name": ph.file_name, "pattern": pat.id()}));
            }
            None => {
                unknown += 1;
                files.push(json!({"id": id.0, "name": ph.file_name, "pattern": "UNKNOWN"}));
            }
        }
    }

    let dominant_from = ScreenshotPattern::CONVERTIBLE
        .iter()
        .copied()
        .max_by_key(|pat| counts.get(pat.id()).copied().unwrap_or(0))
        .filter(|pat| counts.get(pat.id()).copied().unwrap_or(0) > 0)
        .unwrap_or(ScreenshotPattern::SharexSpectacle)
        .id();

    let mut count_list: Vec<Value> = ScreenshotPattern::ALL
        .iter()
        .filter_map(|pat| {
            let c = counts.get(pat.id()).copied().unwrap_or(0);
            (c > 0).then(|| {
                json!({
                    "pattern": pat.id(),
                    "label": pat.label(),
                    "count": c,
                    "convertible": pat.is_convertible(),
                })
            })
        })
        .collect();
    if unknown > 0 {
        count_list.push(json!({
            "pattern": "UNKNOWN",
            "label": "Other / Unrecognized",
            "count": unknown,
            "convertible": false,
        }));
    }

    let distinct_patterns = count_list.len();
    let convertible_options: Vec<Value> = ScreenshotPattern::CONVERTIBLE.iter().map(|pat| json!({"id": pat.id(), "label": pat.label()})).collect();

    Ok(json!({
        "counts": count_list,
        "distinctPatterns": distinct_patterns,
        "mixedWarning": distinct_patterns > 1,
        "dominantFrom": dominant_from,
        "convertiblePatterns": convertible_options,
        "files": files,
    }))
}

fn convert_screenshots(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "organizer.convertScreenshots";
    let from_str = str_param(p, "fromPattern").ok_or_else(|| bad(C, "missing `fromPattern`"))?;
    let to_str = str_param(p, "toPattern").ok_or_else(|| bad(C, "missing `toPattern`"))?;
    let from_pat = ScreenshotPattern::from_id(from_str).ok_or_else(|| bad(C, format!("unknown pattern `{from_str}`")))?;
    let to_pat = ScreenshotPattern::from_id(to_str).ok_or_else(|| bad(C, format!("unknown pattern `{to_str}`")))?;
    if !to_pat.is_convertible() {
        return Err(bad(C, format!("pattern `{to_str}` cannot be used as a target format")));
    }
    let preview = bool_or(p, "preview", false);
    let ids = target_photos_for_screenshots(s, p)?;

    let mut taken: HashSet<String> = HashSet::new();
    let mut plans: Vec<RenamePlan> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut mismatched: Vec<Value> = Vec::new();

    for id in ids {
        let Some(ph) = s.catalog.photo(id) else { continue };
        let (stem, ext) = split_ext(&ph.file_name);
        if let Some(ts) = match_pattern(stem, from_pat) {
            let dir = match &ph.source {
                Source::File { path } => Path::new(path).parent().map(Path::to_path_buf),
                Source::Demo { .. } => None,
            };
            let mut k = 0usize;
            let (new_name, new_path) = loop {
                let new_stem = format_screenshot_stem(ts, to_pat, k);
                let cand_name = if ext.is_empty() { new_stem } else { format!("{new_stem}.{ext}") };
                match (&ph.source, &dir) {
                    (Source::File { path }, Some(d)) => {
                        let cand_path = d.join(&cand_name);
                        let cand_str = cand_path.to_string_lossy().to_string();
                        let key = cand_str.to_lowercase();
                        let same = Path::new(path) == cand_path.as_path();
                        if !taken.contains(&key) && (same || !cand_path.exists()) {
                            taken.insert(key);
                            break (cand_name, Some(cand_str));
                        }
                    }
                    _ => {
                        let key = format!("demo:{}", cand_name.to_lowercase());
                        if taken.insert(key) {
                            break (cand_name, None);
                        }
                    }
                }
                k = k.saturating_add(1);
            };
            let from_path = match &ph.source {
                Source::File { path } => Some(path.clone()),
                Source::Demo { .. } => None,
            };
            rows.push(json!({
                "id": id.0,
                "from": ph.file_name,
                "to": new_name,
                "changed": ph.file_name != new_name,
            }));
            if ph.file_name != new_name {
                plans.push(RenamePlan { id: id.0, from: ph.file_name.clone(), to: new_name, from_path, to_path: new_path });
            }
        } else {
            let detected = detect_stem_pattern(stem).map(|d| d.id()).unwrap_or("UNKNOWN");
            mismatched.push(json!({
                "id": id.0,
                "name": ph.file_name,
                "detectedPattern": detected,
            }));
            rows.push(json!({
                "id": id.0,
                "from": ph.file_name,
                "to": Value::Null,
                "changed": false,
                "detectedPattern": detected,
            }));
        }
    }

    let renamed = if preview || plans.is_empty() {
        plans.len()
    } else {
        s.apply_file_plans(&RealFs, &plans, &|n| format!("Convert {n} Screenshot Name{}", if n == 1 { "" } else { "s" }))?
    };

    Ok(json!({
        "preview": preview,
        "count": renamed,
        "rows": rows,
        "plans": plans,
        "mismatched": mismatched,
    }))
}

// ---------- MyStuff MongoDB integration ----------

fn find_python_for_pymongo() -> &'static str {
    let shirogane_venv = "/home/berschi/Coding/shirogane/.venv/bin/python";
    if Path::new(shirogane_venv).is_file() { shirogane_venv } else { "python3" }
}

fn mystuff_load_games(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "organizer.mystuffLoadGames";
    let host = str_param(p, "host").unwrap_or(&s.organizer.mystuff_host).trim().to_string();
    if host.is_empty() {
        return Err(bad(C, "missing `host`"));
    }
    s.organizer.mystuff_host = host.clone();

    if let Some(mock) = s.organizer.mock_games.get(&host) {
        return Ok(json!({ "host": host, "games": mock }));
    }

    let script = r#"
import json, sys
from pymongo import MongoClient
host = sys.argv[1]
client = MongoClient(f"mongodb://root:password@{host}:27017/", serverSelectionTimeoutMS=3000)
db = client["mystuff"]
games = []
for doc in db["games"].find({}, {"name": 1, "media": 1}):
    name = str(doc.get("name", "")).strip()
    if not name:
        continue
    raw_media = doc.get("media") or []
    media = []
    if isinstance(raw_media, list):
        for m in raw_media:
            if isinstance(m, dict):
                media.append({"name": str(m.get("name", "")), "location": str(m.get("location", ""))})
    games.append({"id": str(doc.get("_id", "")), "name": name, "media": media})
games.sort(key=lambda g: g["name"].lower())
print(json.dumps(games))
"#;

    let out = std::process::Command::new(find_python_for_pymongo())
        .arg("-c")
        .arg(script)
        .arg(&host)
        .output()
        .map_err(|e| EngineError::Other(format!("failed to run MongoDB helper: {e}")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(EngineError::Other(format!("MongoDB connection to `{host}` failed: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let games: Vec<MyStuffGameEntry> =
        serde_json::from_str(stdout.trim()).map_err(|e| EngineError::Other(format!("invalid response from MongoDB helper: {e}")))?;

    Ok(json!({
        "host": host,
        "games": games,
    }))
}

fn mystuff_save_media(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "organizer.mystuffSaveMedia";
    let host = str_param(p, "host").unwrap_or(&s.organizer.mystuff_host).trim().to_string();
    let game_name = str_param(p, "gameName").ok_or_else(|| bad(C, "missing `gameName`"))?.trim().to_string();
    let game_id = str_param(p, "gameId").unwrap_or("").trim().to_string();
    let media_val = p.get("media").and_then(Value::as_array).ok_or_else(|| bad(C, "`media` must be an array of {name, location}"))?;

    let mut media: Vec<MyStuffMediaEntry> = Vec::new();
    for item in media_val {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let location = item.get("location").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if !name.is_empty() || !location.is_empty() {
            media.push(MyStuffMediaEntry { name, location });
        }
    }

    if let Some(mock_list) = s.organizer.mock_games.get_mut(&host) {
        if let Some(g) = mock_list.iter_mut().find(|g| (!game_id.is_empty() && g.id == game_id) || g.name == game_name) {
            g.media = media.clone();
            return Ok(json!({ "saved": true, "host": host, "gameName": game_name, "media": media }));
        }
        return Err(bad(C, format!("no game `{game_name}` on `{host}`")));
    }

    let payload = json!({
        "host": host,
        "gameId": game_id,
        "gameName": game_name,
        "media": media,
    });

    let script = r#"
import json, sys
from bson import ObjectId
from pymongo import MongoClient
req = json.loads(sys.argv[1])
client = MongoClient(f"mongodb://root:password@{req['host']}:27017/", serverSelectionTimeoutMS=3000)
col = client["mystuff"]["games"]
query = {"name": req["gameName"]}
if req.get("gameId"):
    try:
        query = {"_id": ObjectId(req["gameId"])}
    except Exception:
        pass
res = col.update_one(query, {"$set": {"media": req["media"]}})
if res.matched_count == 0 and query != {"name": req["gameName"]}:
    res = col.update_one({"name": req["gameName"]}, {"$set": {"media": req["media"]}})
print(json.dumps({"matched": res.matched_count, "modified": res.modified_count}))
"#;

    let out = std::process::Command::new(find_python_for_pymongo())
        .arg("-c")
        .arg(script)
        .arg(payload.to_string())
        .output()
        .map_err(|e| EngineError::Other(format!("failed to run MongoDB helper: {e}")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(EngineError::Other(format!("MongoDB update on `{host}` failed: {stderr}")));
    }

    Ok(json!({
        "saved": true,
        "host": host,
        "gameName": game_name,
        "media": media,
    }))
}

pub fn has_staged_moves(s: &Session) -> std::result::Result<(), String> {
    if s.organizer.staged_moves.keys().any(|id| s.catalog.photo(*id).is_some_and(|p| !p.deleted)) {
        Ok(())
    } else {
        Err("no photos have a staged destination subfolder (press M to assign one)".into())
    }
}

/// Filter `visible` in place to keep only photos that have horizontal black bars (letterbox).
pub fn filter_letterbox_photos(s: &mut Session, visible: &mut Vec<PhotoId>) {
    use rayon::prelude::*;

    // 1. Gather uncached photos with a file source
    let uncached: Vec<(PhotoId, String)> = visible
        .iter()
        .copied()
        .filter(|id| !s.organizer.letterbox_cache.contains_key(id))
        .filter_map(|id| {
            let p = s.catalog.photo(id)?;
            match &p.source {
                Source::File { path } => Some((id, path.clone())),
                Source::Demo { .. } => None,
            }
        })
        .collect();

    if !uncached.is_empty() {
        let detected: Vec<(PhotoId, bool)> = uncached
            .par_iter()
            .map(|(id, path)| {
                let is_lb = std::fs::read(path)
                    .ok()
                    .and_then(|bytes| lightcraft_codecs::decode_thumbnail(&bytes, 128).ok())
                    .map(|thumb| {
                        let orientation = lightcraft_geom::Orientation::from_exif(thumb.orientation);
                        thumb.image.oriented(orientation).is_letterboxed()
                    })
                    .unwrap_or(false);
                (*id, is_lb)
            })
            .collect();

        for (id, is_lb) in detected {
            s.organizer.letterbox_cache.insert(id, is_lb);
        }
    }

    // 2. Fallback for any remaining photos (e.g. demo scenes or decodes needing pipeline)
    for id in visible.iter() {
        if !s.organizer.letterbox_cache.contains_key(id) {
            let is_lb = s.source_now(*id, crate::media::SourceLevel::Thumb).map(|img| img.is_letterboxed()).unwrap_or(false);
            s.organizer.letterbox_cache.insert(*id, is_lb);
        }
    }

    // 3. Keep only letterboxed photos that have not been cropped to remove bars
    visible.retain(|id| {
        if let Some(p) = s.catalog.photo(*id)
            && !p.develop.crop.geometry.is_identity()
        {
            return false;
        }
        s.organizer.letterbox_cache.get(id).copied().unwrap_or(false)
    });
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "organizer.state",
            "Media Organizer State",
            [],
            None,
            "{} → {sourceFolder, matchedTargetRoot, targetRoots, targetFolders, targetFolder, subfolders, recentSubfolders, stagedMoves, stagedCount, showSubfoldersOnImages, pigalleryUrl, mappings, mystuffHost}",
            always,
            |s, _| Ok(state_json(s))
        ),
        cmd!(
            "organizer.setTargetFolder",
            "Set Organizer Target Folder",
            [],
            None,
            "{path: string, create?: bool} — sets the destination root folder for staged subfolder moves",
            always,
            |s, p| {
                const C: &str = "organizer.setTargetFolder";
                let path = trim_slashes(str_param(p, "path").ok_or_else(|| bad(C, "missing `path`"))?);
                if path.is_empty() {
                    return Err(bad(C, "`path` is empty"));
                }
                let create = bool_or(p, "create", false);
                let dir = Path::new(&path);
                if create && !dir.exists() {
                    std::fs::create_dir_all(dir).map_err(|e| EngineError::Other(format!("create {path}: {e}")))?;
                    s.organizer.invalidate_folder_cache();
                }
                s.organizer.target_folder = Some(path);
                let _ = s.save_prefs();
                Ok(state_json(s))
            }
        ),
        cmd!(
            "organizer.createSubfolder",
            "Create Organizer Subfolder",
            [],
            None,
            "{subfolder: string, assignToSelection?: bool (default true)} — creates `<targetFolder>/<subfolder>` on disk and optionally stages it for the selected photos",
            always,
            |s, p| {
                const C: &str = "organizer.createSubfolder";
                if s.organizer.target_folder.is_none() {
                    sync_target_folder_for_source(s);
                }
                let target = s
                    .organizer
                    .target_folder
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .ok_or_else(|| bad(C, "select a target folder first (`organizer.setTargetFolder`)"))?;
                let raw_sub = str_param(p, "subfolder").ok_or_else(|| bad(C, "missing `subfolder`"))?;
                let sub = validate_subfolder(raw_sub).map_err(|e| bad(C, e))?;
                if sub == "." {
                    return Err(bad(C, "subfolder name cannot be empty"));
                }
                let full = Path::new(&target).join(&sub);
                std::fs::create_dir_all(&full).map_err(|e| EngineError::Other(format!("create {}: {e}", full.display())))?;
                remember_recent_subfolder(s, &sub);
                s.organizer.note_subfolder_created(&target, &sub);
                let assign = bool_or(p, "assignToSelection", true);
                let mut assigned = 0usize;
                if assign && let Ok(ids) = s.targets(p) {
                    for id in ids {
                        s.organizer.staged_moves.insert(id, sub.clone());
                        assigned += 1;
                    }
                }
                let _ = s.save_prefs();
                let subfolders = s.organizer.subfolders_for(&target).to_vec();
                Ok(json!({
                    "created": full.to_string_lossy(),
                    "subfolder": sub,
                    "assigned": assigned,
                    "subfolders": subfolders,
                }))
            }
        ),
        CommandSpec {
            explicit_targets: true,
            ..cmd!(
                "organizer.stageSubfolder",
                "Assign Destination Subfolder",
                [],
                None,
                "{subfolder: string|null, ids?: [photo]} — stages the relative destination subfolder (e.g. `_Lea/alleine`, or `.` for the target folder root; empty/null clears) for the selected or named photos",
                has_selection,
                |s, p| {
                    const C: &str = "organizer.stageSubfolder";
                    let ids = s.targets(p)?;
                    let sub_opt = match p.get("subfolder") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(st)) if st.trim().is_empty() => None,
                        Some(Value::String(st)) => Some(validate_subfolder(st).map_err(|e| bad(C, e))?),
                        _ => return Err(bad(C, "`subfolder` must be a string or null")),
                    };
                    let mut count = 0usize;
                    for id in ids {
                        match &sub_opt {
                            Some(sub) => {
                                s.organizer.staged_moves.insert(id, sub.clone());
                                count += 1;
                            }
                            None => {
                                if s.organizer.staged_moves.remove(&id).is_some() {
                                    count += 1;
                                }
                            }
                        }
                    }
                    if let Some(sub) = &sub_opt {
                        remember_recent_subfolder(s, sub);
                    }
                    Ok(json!({
                        "changed": count,
                        "subfolder": sub_opt,
                        "stagedCount": s.organizer.staged_moves.len(),
                    }))
                }
            )
        },
        cmd!(
            "organizer.deleteFolder",
            "Delete Empty Folder",
            [],
            None,
            "{path?: string} — deletes the empty folder on disk (defaults to current source folder) and browses its parent",
            always,
            |s, p| {
                const C: &str = "organizer.deleteFolder";
                let raw_path = str_param(p, "path").map(trim_slashes);
                let path = match raw_path {
                    Some(p) => p,
                    None => current_source_folder(s).ok_or_else(|| bad(C, "no source folder to delete"))?,
                };
                let dir = Path::new(&path);
                if !dir.exists() {
                    return Err(bad(C, format!("{path}: folder does not exist")));
                }
                if !dir.is_dir() {
                    return Err(bad(C, format!("{path}: not a directory")));
                }
                if !is_safe_to_delete_folder(dir) {
                    return Err(bad(C, format!("{path}: cannot delete root or home directory")));
                }
                if !is_folder_empty(dir) {
                    return Err(bad(C, format!("{path}: folder is not empty")));
                }

                std::fs::remove_dir_all(dir).map_err(|e| EngineError::Other(format!("delete {path}: {e}")))?;
                s.organizer.invalidate_folder_cache();

                let mut next_folder = None;
                if s.browse.as_ref().is_some_and(|b| trim_slashes(&b.path) == path) {
                    if let Some(parent) = dir.parent().filter(|p| p.is_dir()) {
                        let parent_str = parent.to_string_lossy().to_string();
                        let _ = s.execute("library.browse", &json!({"path": parent_str}));
                        next_folder = Some(parent_str);
                    } else if let Some(src) = s.organizer.mappings.first().filter(|m| Path::new(&m.source).is_dir()).map(|m| m.source.clone()) {
                        let _ = s.execute("library.browse", &json!({"path": src}));
                        next_folder = Some(src);
                    } else {
                        s.browse = None;
                        s.source = LibrarySource::All;
                        s.keep_active_in_view();
                    }
                } else if s.library_folder.as_ref().is_some_and(|f| trim_slashes(f) == path) {
                    s.library_folder = None;
                    s.source = LibrarySource::All;
                    s.keep_active_in_view();
                }

                Ok(json!({
                    "deleted": path,
                    "nextFolder": next_folder,
                }))
            }
        ),
        cmd!(
            "organizer.clearStaged",
            "Clear Staged Subfolder Moves",
            [],
            None,
            "{ids?: [photo]} — clears staged destination subfolders (for `ids` if given, else all)",
            always,
            |s, p| {
                if super::names_photos(p) {
                    for id in s.targets(p)? {
                        s.organizer.staged_moves.remove(&id);
                    }
                } else {
                    s.organizer.staged_moves.clear();
                }
                Ok(json!({"stagedCount": s.organizer.staged_moves.len()}))
            }
        ),
        cmd!(
            "organizer.setShowSubfolders",
            "Show Staged Subfolders on Photos",
            [],
            None,
            "{show?: bool (default: toggle)} — show or hide the staged destination subfolder badge on thumbnails and fullscreen view",
            always,
            |s, p| {
                let show = bool_or(p, "show", !s.organizer.show_subfolders_on_images);
                s.organizer.show_subfolders_on_images = show;
                let _ = s.save_prefs();
                Ok(json!({"showSubfoldersOnImages": show}))
            }
        ),
        cmd!(
            "organizer.setShowTagsInGrid",
            "Show Tags in Grid",
            [],
            None,
            "{show?: bool (default: toggle)} — show or hide the tags bar beneath the badge bar on thumbnails in the grid",
            always,
            |s, p| {
                let show = bool_or(p, "show", !s.organizer.show_tags_in_grid);
                s.organizer.show_tags_in_grid = show;
                let _ = s.save_prefs();
                Ok(json!({"showTagsInGrid": show}))
            }
        ),
        cmd!(
            "organizer.setTrashOnDelete",
            "Move to Trash on Delete",
            [],
            None,
            "{enabled?: bool (default: toggle)} — move photo files and sidecars to system trash when deleting photos",
            always,
            |s, p| {
                let enabled = bool_or(p, "enabled", !s.organizer.trash_on_delete);
                s.organizer.trash_on_delete = enabled;
                let _ = s.save_prefs();
                Ok(json!({"trashOnDelete": enabled}))
            }
        ),
        cmd!(
            "organizer.setColoredRatingStars",
            "Color Rating Stars by Level",
            [],
            None,
            "{enabled?: bool (default: toggle)} — color rating stars (3 = yellow, 4 = orange, 5 = red) in the grid",
            always,
            |s, p| {
                let enabled = bool_or(p, "enabled", !s.organizer.colored_rating_stars);
                s.organizer.colored_rating_stars = enabled;
                let _ = s.save_prefs();
                Ok(json!({"coloredRatingStars": enabled}))
            }
        ),
        cmd!(
            "organizer.setShowRatingInFullscreen",
            "Show Rating Stars in Fullscreen",
            [],
            None,
            "{enabled?: bool (default: toggle)} — show rating stars in the top-left corner in fullscreen preview",
            always,
            |s, p| {
                let enabled = bool_or(p, "enabled", !s.organizer.show_rating_in_fullscreen);
                s.organizer.show_rating_in_fullscreen = enabled;
                let _ = s.save_prefs();
                Ok(json!({"showRatingInFullscreen": enabled}))
            }
        ),
        cmd!(
            "organizer.setFilterLetterbox",
            "Filter Letterbox Images",
            [],
            None,
            "{enabled?: bool (default: toggle)} — filter displayed photos to only those with horizontal black bars (letterbox)",
            always,
            |s, p| {
                let enabled = bool_or(p, "enabled", !s.organizer.filter_letterbox);
                s.organizer.filter_letterbox = enabled;
                let _ = s.save_prefs();
                s.refresh_view();
                let matched = s.visible().len();
                Ok(json!({"filterLetterbox": enabled, "matched": matched}))
            }
        ),
        cmd!(
            "organizer.setMapping",
            "Configure Source → Target Root Mapping",
            [],
            None,
            "{source: string, targetRoot?: string, remove?: bool} — adds, updates, or removes a Todo-folder → Target-root mapping",
            always,
            |s, p| {
                const C: &str = "organizer.setMapping";
                let src = trim_slashes(str_param(p, "source").ok_or_else(|| bad(C, "missing `source`"))?);
                if src.is_empty() {
                    return Err(bad(C, "`source` cannot be empty"));
                }
                let remove = bool_or(p, "remove", false);
                s.organizer.mappings.retain(|m| trim_slashes(&m.source) != src);
                if !remove {
                    let target_root = trim_slashes(str_param(p, "targetRoot").ok_or_else(|| bad(C, "missing `targetRoot`"))?);
                    if target_root.is_empty() {
                        return Err(bad(C, "`targetRoot` cannot be empty"));
                    }
                    s.organizer.mappings.push(FolderMapping { source: src, target_root });
                }
                s.organizer.invalidate_folder_cache();
                let _ = s.save_prefs();
                Ok(state_json(s))
            }
        ),
        CommandSpec {
            explicit_targets: true,
            ..cmd!(
                "organizer.moveImages",
                "Move Staged Images to Target Subfolders",
                ["View", "Organizer"],
                Some("Ctrl+Alt+M"),
                "{targetFolder?: string, subfolder?: string, ids?: [photo]} — moves all photos with a staged destination subfolder into `<targetFolder>/<subfolder>` (with their XMP sidecars and `_1` collision suffix; one Undo step)",
                has_staged_moves,
                move_images
            )
        },
        cmd!(
            query "organizer.detectScreenshots",
            "Detect Screenshot Filename Patterns",
            [],
            None,
            "{ids?: [photo], allVisible?: bool} → {counts, distinctPatterns, mixedWarning, dominantFrom, convertiblePatterns, files}",
            always,
            detect_screenshots
        ),
        CommandSpec {
            explicit_targets: true,
            ..cmd!(
                "organizer.convertScreenshots",
                "Convert Screenshot Filenames",
                [],
                None,
                "{fromPattern: SHAREX_SPECTACLE|NVIDIA|AMD|STEAM|CAMERA|…, toPattern: SHAREX_SPECTACLE|NVIDIA|AMD|STEAM|CAMERA, preview?: bool, ids?: [photo], allVisible?: bool} — converts screenshot filenames between formats (Undoable)",
                always,
                convert_screenshots
            )
        },
        cmd!(
            query "organizer.pigalleryUrl",
            "PiGallery2 URL for Folder",
            [],
            None,
            "{path?: string} → {path, url}",
            always,
            |s, p| {
                if s.organizer.target_folder.is_none() {
                    sync_target_folder_for_source(s);
                }
                let src = current_source_folder(s);
                let path = str_param(p, "path")
                    .map(trim_slashes)
                    .or_else(|| s.organizer.target_folder.clone())
                    .or(src);
                let url = pigallery_url_for_path(s, path.as_deref());
                Ok(json!({ "path": path, "url": url }))
            }
        ),
        cmd!(
            query "organizer.mystuffLoadGames",
            "Load MyStuff Games from MongoDB",
            [],
            None,
            "{host?: \"shirogane\"|\"127.0.0.1\"} → {host, games: [{id, name, media: [{name, location}]}]}",
            always,
            mystuff_load_games
        ),
        cmd!(
            "organizer.mystuffSaveMedia",
            "Save PiGallery2 Media to MyStuff MongoDB",
            [],
            None,
            "{host?: string, gameId?: string, gameName: string, media: [{name, location}]} → {saved, host, gameName, media}",
            always,
            mystuff_save_media
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_patterns_parse_and_convert_with_collisions() {
        // ShareX -> Steam, NVIDIA, AMD, Camera
        let sharex = "2026-03-15_19.42.08.350";
        assert_eq!(detect_stem_pattern(sharex), Some(ScreenshotPattern::SharexSpectacle));
        let ts = match_pattern(sharex, ScreenshotPattern::SharexSpectacle).unwrap();
        assert_eq!(format_screenshot_stem(ts, ScreenshotPattern::Steam, 0), "20260315194208_1");
        assert_eq!(format_screenshot_stem(ts, ScreenshotPattern::Steam, 1), "20260315194208_2");
        assert_eq!(format_screenshot_stem(ts, ScreenshotPattern::Nvidia, 0), "2026.03.15 - 19.42.08.00");
        assert_eq!(format_screenshot_stem(ts, ScreenshotPattern::Nvidia, 2), "2026.03.15 - 19.42.08 - (2).00");
        assert_eq!(format_screenshot_stem(ts, ScreenshotPattern::Amd, 0), "2026.03.15-19.42");
        assert_eq!(format_screenshot_stem(ts, ScreenshotPattern::Amd, 1), "2026.03.15-19.42_1");
        assert_eq!(format_screenshot_stem(ts, ScreenshotPattern::Camera, 0), "20260315_194208");

        // Steam -> ShareX
        let steam = "20260315194208_1";
        assert_eq!(detect_stem_pattern(steam), Some(ScreenshotPattern::Steam));
        let ts2 = match_pattern(steam, ScreenshotPattern::Steam).unwrap();
        assert_eq!(format_screenshot_stem(ts2, ScreenshotPattern::SharexSpectacle, 0), "2026-03-15_19.42.08.000");
        assert_eq!(format_screenshot_stem(ts2, ScreenshotPattern::SharexSpectacle, 3), "2026-03-15_19.42.08.003");
    }

    #[test]
    fn pigallery_url_encodes_relative_paths_for_media_photos_and_videos() {
        let s = Session::new();
        assert_eq!(
            pigallery_url_for_path(&s, Some("/home/berschi/syncthing/Pictures/Fotos/Fotos/2026/_Lea/alleine")),
            Some("https://shirogane.fritz.box/pigallery2/photos/gallery/2026%2F_Lea%2Falleine".to_string())
        );
        assert_eq!(
            pigallery_url_for_path(&s, Some("/home/berschi/syncthing/Pictures/Media/Games/Hollow Knight")),
            Some("https://shirogane.fritz.box/pigallery2/media/gallery/Games%2FHollow%20Knight".to_string())
        );
        assert_eq!(
            pigallery_url_for_path(&s, Some("/mnt/HDD/syncthing_HDD/Videos/Clips 2026")),
            Some("https://shirogane.fritz.box/pigallery2/videos/gallery/Clips%202026".to_string())
        );
    }

    #[test]
    fn stage_and_move_images_creates_subfolders_moves_xmp_handles_collisions_and_undoes() {
        let tmp = std::env::temp_dir().join(format!("lc-organizer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let todo_dir = tmp.join("_todo_photos");
        let target_dir = tmp.join("Fotos").join("2026");
        std::fs::create_dir_all(&todo_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();

        let p1 = todo_dir.join("2026-03-15_19.42.08.350.jpg");
        let p1_xmp = todo_dir.join("2026-03-15_19.42.08.350.xmp");
        let p2 = todo_dir.join("2026-03-15_19.42.09.100.jpg");
        std::fs::write(&p1, b"fake-jpg-1").unwrap();
        std::fs::write(&p1_xmp, b"<x:xmpmeta/>").unwrap();
        std::fs::write(&p2, b"fake-jpg-2").unwrap();

        // Pre-create a colliding file in destination to verify _1 suffix
        let sub_dir = target_dir.join("_Lea").join("alleine");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(sub_dir.join("2026-03-15_19.42.09.100.jpg"), b"existing").unwrap();

        let mut s = Session::new();
        for (id, p) in [(1, &p1), (2, &p2)] {
            let path = p.to_string_lossy().to_string();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let photo = lightcraft_catalog::Photo::new(PhotoId(id), Source::File { path }, &name, "JPG", 1920, 1080, "2026-03-15T19:42:08");
            s.catalog.apply(lightcraft_catalog::Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        }
        s.execute("library.select", &json!({"ids": [1, 2]})).unwrap();

        // Set target folder and stage subfolder
        s.execute("organizer.setTargetFolder", &json!({"path": target_dir.to_string_lossy()})).unwrap();
        s.execute("organizer.stageSubfolder", &json!({"subfolder": "_Lea/alleine"})).unwrap();
        assert_eq!(s.organizer.staged_moves.len(), 2);

        // Move images
        let moved = s.execute("organizer.moveImages", &json!({})).unwrap();
        assert_eq!(moved["moved"], 2);
        assert!(s.organizer.staged_moves.is_empty());

        let dest1 = sub_dir.join("2026-03-15_19.42.08.350.jpg");
        let dest1_xmp = sub_dir.join("2026-03-15_19.42.08.350.xmp");
        let dest2 = sub_dir.join("2026-03-15_19.42.09.100_1.jpg");
        assert!(dest1.exists());
        assert!(dest1_xmp.exists());
        assert!(dest2.exists());
        // JPEGs get no sidecar; their metadata goes into the file. These fixtures aren't real JPEGs, so
        // the write fails, is reported, and the moved files stay byte-for-byte intact.
        assert!(!sub_dir.join("2026-03-15_19.42.09.100_1.xmp").exists());
        assert_eq!(moved["sidecarsFailed"].as_array().map(Vec::len), Some(0));
        assert_eq!(moved["imageFailed"].as_array().map(Vec::len), Some(2));
        assert_eq!(std::fs::read(&dest2).unwrap(), b"fake-jpg-2");

        // Undo moves both files and the XMP sidecar back!
        s.execute("edit.undo", &json!({})).unwrap();
        assert!(p1.exists());
        assert!(p1_xmp.exists());
        assert!(p2.exists());

        // Screenshot detection & conversion
        let det = s.execute("organizer.detectScreenshots", &json!({"allVisible": true})).unwrap();
        assert_eq!(det["dominantFrom"], "SHAREX_SPECTACLE");
        let conv =
            s.execute("organizer.convertScreenshots", &json!({"fromPattern": "SHAREX_SPECTACLE", "toPattern": "STEAM", "allVisible": true})).unwrap();
        assert_eq!(conv["count"], 2);
        assert!(todo_dir.join("20260315194208_1.jpg").exists());
        assert!(todo_dir.join("20260315194209_1.jpg").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn delete_empty_folder_removes_folder_and_navigates_to_parent_and_rejects_non_empty() {
        let tmp = std::env::temp_dir().join(format!("lc-del-folder-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let parent = tmp.join("parent");
        let empty_sub = parent.join("empty_sub");
        let non_empty_sub = parent.join("non_empty_sub");
        std::fs::create_dir_all(&empty_sub).unwrap();
        std::fs::create_dir_all(&non_empty_sub).unwrap();
        std::fs::write(non_empty_sub.join("photo.jpg"), b"fake").unwrap();

        assert!(is_folder_empty(&empty_sub));
        assert!(!is_folder_empty(&non_empty_sub));

        // Folder with .DS_Store is still considered empty
        std::fs::write(empty_sub.join(".DS_Store"), b"junk").unwrap();
        assert!(is_folder_empty(&empty_sub));

        let mut s = Session::new().with_fs();
        s.execute("library.browse", &json!({"path": empty_sub.to_string_lossy()})).unwrap();
        assert_eq!(current_source_folder(&s).as_deref(), Some(empty_sub.to_string_lossy().as_ref()));

        // Trying to delete non-empty folder returns error
        let err = s.execute("organizer.deleteFolder", &json!({"path": non_empty_sub.to_string_lossy()}));
        assert!(err.is_err());
        assert!(non_empty_sub.exists());

        // Deleting empty folder succeeds and browses parent
        let res = s.execute("organizer.deleteFolder", &json!({})).unwrap();
        assert_eq!(res["deleted"], empty_sub.to_string_lossy().as_ref());
        assert_eq!(res["nextFolder"], parent.to_string_lossy().as_ref());
        assert!(!empty_sub.exists());
        assert_eq!(current_source_folder(&s).as_deref(), Some(parent.to_string_lossy().as_ref()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn organizer_set_show_tags_in_grid_toggles() {
        let mut s = Session::new();
        assert!(!s.organizer.show_tags_in_grid);
        let res = s.execute("organizer.setShowTagsInGrid", &json!({"show": true})).unwrap();
        assert_eq!(res["showTagsInGrid"], true);
        assert!(s.organizer.show_tags_in_grid);

        let res2 = s.execute("organizer.setShowTagsInGrid", &json!({})).unwrap();
        assert_eq!(res2["showTagsInGrid"], false);
        assert!(!s.organizer.show_tags_in_grid);
    }

    #[test]
    fn organizer_set_trash_on_delete_toggles() {
        let mut s = Session::new();
        assert!(!s.organizer.trash_on_delete);
        let res = s.execute("organizer.setTrashOnDelete", &json!({"enabled": true})).unwrap();
        assert_eq!(res["trashOnDelete"], true);
        assert!(s.organizer.trash_on_delete);

        let res2 = s.execute("organizer.setTrashOnDelete", &json!({})).unwrap();
        assert_eq!(res2["trashOnDelete"], false);
        assert!(!s.organizer.trash_on_delete);
    }

    #[test]
    fn organizer_set_colored_rating_stars_toggles() {
        let mut s = Session::new();
        assert!(!s.organizer.colored_rating_stars);
        let res = s.execute("organizer.setColoredRatingStars", &json!({"enabled": true})).unwrap();
        assert_eq!(res["coloredRatingStars"], true);
        assert!(s.organizer.colored_rating_stars);

        let res2 = s.execute("organizer.setColoredRatingStars", &json!({})).unwrap();
        assert_eq!(res2["coloredRatingStars"], false);
        assert!(!s.organizer.colored_rating_stars);
    }

    #[test]
    fn organizer_set_show_rating_in_fullscreen_toggles() {
        let mut s = Session::new();
        assert!(!s.organizer.show_rating_in_fullscreen);
        let res = s.execute("organizer.setShowRatingInFullscreen", &json!({"enabled": true})).unwrap();
        assert_eq!(res["showRatingInFullscreen"], true);
        assert!(s.organizer.show_rating_in_fullscreen);

        let res2 = s.execute("organizer.setShowRatingInFullscreen", &json!({})).unwrap();
        assert_eq!(res2["showRatingInFullscreen"], false);
        assert!(!s.organizer.show_rating_in_fullscreen);
    }

    #[test]
    fn organizer_set_filter_letterbox_toggles() {
        let mut s = Session::new();
        assert!(!s.organizer.filter_letterbox);
        let res = s.execute("organizer.setFilterLetterbox", &json!({"enabled": true})).unwrap();
        assert_eq!(res["filterLetterbox"], true);
        assert!(s.organizer.filter_letterbox);

        let res2 = s.execute("organizer.setFilterLetterbox", &json!({})).unwrap();
        assert_eq!(res2["filterLetterbox"], false);
        assert!(!s.organizer.filter_letterbox);
    }

    #[test]
    fn organizer_filter_letterbox_filters_visible_photos() {
        use lightcraft_raster::Rgba8;

        let tmp = std::env::temp_dir().join(format!("lc-lb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // 1. Create a letterboxed image (top & bottom black, middle bright)
        let w = 100usize;
        let h = 100usize;
        let lb_img = Rgba8::from_fn(w, h, |_x, y| if (10..90).contains(&y) { [200, 200, 200, 255] } else { [0, 0, 0, 255] });
        let p_lb = tmp.join("letterboxed.jpg");
        let lb_bytes =
            lightcraft_codecs::encode_jpeg(&lightcraft_codecs::EncodeImage::rgba8(&lb_img), 95, Default::default(), &Default::default()).unwrap();
        std::fs::write(&p_lb, &lb_bytes).unwrap();

        // 2. Create a non-letterboxed image (solid bright)
        let non_lb_img = Rgba8::filled(w, h, [200, 200, 200, 255]);
        let p_non_lb = tmp.join("normal.jpg");
        let non_lb_bytes =
            lightcraft_codecs::encode_jpeg(&lightcraft_codecs::EncodeImage::rgba8(&non_lb_img), 95, Default::default(), &Default::default()).unwrap();
        std::fs::write(&p_non_lb, &non_lb_bytes).unwrap();

        let mut s = Session::new();
        let ph1 = lightcraft_catalog::Photo::new(
            PhotoId(1),
            Source::File { path: p_lb.to_string_lossy().to_string() },
            "letterboxed.jpg",
            "JPG",
            w as u32,
            h as u32,
            "2026-03-15T19:42:08",
        );
        let ph2 = lightcraft_catalog::Photo::new(
            PhotoId(2),
            Source::File { path: p_non_lb.to_string_lossy().to_string() },
            "normal.jpg",
            "JPG",
            w as u32,
            h as u32,
            "2026-03-15T19:42:08",
        );
        s.catalog.apply(lightcraft_catalog::Op::AddPhoto { photo: Box::new(ph1) }).unwrap();
        s.catalog.apply(lightcraft_catalog::Op::AddPhoto { photo: Box::new(ph2) }).unwrap();

        // Without filter: both visible
        assert_eq!(s.visible().len(), 2);

        // Turn on letterbox filter: only photo 1 visible
        s.execute("organizer.setFilterLetterbox", &json!({"enabled": true})).unwrap();
        assert_eq!(s.visible(), vec![PhotoId(1)]);

        // Turn off filter: both visible again
        s.execute("organizer.setFilterLetterbox", &json!({"enabled": false})).unwrap();
        assert_eq!(s.visible().len(), 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn photo_delete_with_trash_moves_files_and_sidecars() {
        let tmp = std::env::temp_dir().join(format!("lc-trash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let p1 = tmp.join("test_trash.jpg");
        let p1_xmp = tmp.join("test_trash.xmp");
        std::fs::write(&p1, b"image-data").unwrap();
        std::fs::write(&p1_xmp, b"<x:xmpmeta/>").unwrap();

        let mut s = Session::new();
        let path = p1.to_string_lossy().to_string();
        let photo = lightcraft_catalog::Photo::new(
            PhotoId(1),
            Source::File { path: path.clone() },
            "test_trash.jpg",
            "JPG",
            1920,
            1080,
            "2026-03-15T19:42:08",
        );
        s.catalog.apply(lightcraft_catalog::Op::AddPhoto { photo: Box::new(photo) }).unwrap();

        // Add a virtual copy (copy_of = Some(PhotoId(1)))
        let mut copy = lightcraft_catalog::Photo::new(
            PhotoId(2),
            Source::File { path: path.clone() },
            "test_trash.jpg Copy",
            "JPG",
            1920,
            1080,
            "2026-03-15T19:42:08",
        );
        copy.copy_of = Some(PhotoId(1));
        s.catalog.apply(lightcraft_catalog::Op::AddPhoto { photo: Box::new(copy) }).unwrap();

        // 1. Delete virtual copy with trash: true -> file on disk must NOT be deleted
        s.execute("library.select", &json!({"ids": [2]})).unwrap();
        s.execute("photo.delete", &json!({"ids": [2], "trash": true})).unwrap();
        assert!(p1.exists(), "virtual copy deletion must not delete physical file");
        assert!(p1_xmp.exists(), "virtual copy deletion must not delete sidecar");

        // 2. Delete original with trash: true -> file and sidecar must be trashed
        s.execute("library.select", &json!({"ids": [1]})).unwrap();
        s.execute("photo.delete", &json!({"ids": [1], "trash": true})).unwrap();
        assert!(!p1.exists(), "physical file should be moved to trash");
        assert!(!p1_xmp.exists(), "sidecar should be moved to trash");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
