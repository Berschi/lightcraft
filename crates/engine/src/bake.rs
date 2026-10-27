//! Writing a photo's tags, rating, metadata and develop edits into its JPEG or PNG file when it is
//! moved ("Move Images"). The file is changed in place, so this can't be undone; the move itself can.

use crate::Session;
use crate::export::{ExportFormat, ExportOptions, MetadataPolicy, export_photo};
use crate::sidecar::{OWNED, OWNED_IF_STATED, file_packet, flat_tags};
use lightcraft_catalog::{Op, PhotoId, Source};
use lightcraft_meta::MergeRules;
use std::path::Path;
use std::sync::Arc;

/// JPEG quality of a baked edit: high enough that the re-encode is close to invisible.
const BAKE_JPEG_QUALITY: u8 = 95;

/// Properties replaced in a file (on top of [`OWNED`]): the full keyword paths and the flat tag list,
/// which sidecars don't carry.
const FILE_OWNED: &[&str] = &["lr:hierarchicalSubject", "digiKam:TagsList"];

/// Whether `path` is a JPEG or PNG, the formats edits and metadata can be written into.
pub(crate) fn is_bakeable(path: &str) -> bool {
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    matches!(ext.as_str(), "jpg" | "jpeg" | "png")
}

/// Write photo `id`'s metadata into the file at `path` (its new location, after the move). When its
/// develop settings differ from the neutral defaults, the edits are rendered into the file too,
/// and the catalog then forgets them (they're in the pixels now, so they must not be applied again).
/// On error the file is left as it was.
pub(crate) fn write_into_file(s: &mut Session, id: PhotoId, path: &Path) -> std::result::Result<(), String> {
    let shared = s.catalog.photos().filter(|p| matches!(&p.source, Source::File { path: other } if Path::new(other) == path)).count();
    if shared > 1 {
        return Err("the file is shared by virtual copies, which can't each have their own edits in one file; it was moved without writing its metadata or edits".into());
    }
    let mut ph = Arc::clone(s.catalog.photo(id).ok_or_else(|| "the photo is no longer in the catalog".to_string())?);

    // Read the original file currently on disk before rendering: ensure EXIF/capture date is retained
    let orig_bytes = std::fs::read(path).ok();
    if let Some(ob) = &orig_bytes {
        let orig_meta = lightcraft_meta::extract(ob);
        let captured = if let Some(ct) = orig_meta.capture_time {
            Some(ct.to_iso())
        } else if let Some(cap) = &ph.captured {
            Some(cap.clone())
        } else {
            let stem = path.file_stem().and_then(|st| st.to_str()).unwrap_or_default();
            crate::cmd::organizer::timestamp_from_stem(stem)
        };
        if let Some(iso) = captured
            && ph.captured.as_deref() != Some(&iso)
        {
            let _ = s.apply_system(Op::SetCaptured { id, captured: Some(iso), manual: false });
        }

        let mut m = ph.meta.clone();
        let mut changed_meta = false;
        if m.camera.is_empty()
            && let Some(cam) = orig_meta.model.or(orig_meta.make)
        {
            m.camera = cam;
            changed_meta = true;
        }
        if m.lens.is_empty()
            && let Some(lens) = orig_meta.lens_model
        {
            m.lens = lens;
            changed_meta = true;
        }
        if m.focal_mm.is_none()
            && let Some(f) = orig_meta.focal_length
        {
            m.focal_mm = Some(f as f32);
            changed_meta = true;
        }
        if m.aperture.is_none()
            && let Some(ap) = orig_meta.f_number
        {
            m.aperture = Some(ap as f32);
            changed_meta = true;
        }
        if m.iso.is_none()
            && let Some(iso) = orig_meta.iso
        {
            m.iso = Some(iso);
            changed_meta = true;
        }
        if m.shutter.is_empty()
            && let Some(t) = orig_meta.exposure_time
        {
            m.shutter = if t >= 1.0 { format!("{t:.0}") } else { format!("1/{:.0}", 1.0 / t) };
            changed_meta = true;
        }
        if m.gps.is_none()
            && let Some(g) = orig_meta.gps
        {
            m.gps = Some((g.latitude, g.longitude));
            changed_meta = true;
        }
        if changed_meta {
            let _ = s.apply_system(Op::SetMeta { id, meta: Box::new(m) });
        }

        ph = Arc::clone(s.catalog.photo(id).ok_or_else(|| "the photo is no longer in the catalog".to_string())?);
    }

    let neutral = ph.camera_defaults();
    let render = *ph.develop != neutral;

    let (bytes, width, height) = if render {
        let png = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("png"));
        let opts = ExportOptions {
            format: if png { ExportFormat::Png } else { ExportFormat::Jpeg },
            quality: BAKE_JPEG_QUALITY,
            metadata: MetadataPolicy::All,
            ..Default::default()
        };
        let exported = export_photo(s, id, &opts, 0)?;
        let width = u32::try_from(exported.width).map_err(|_| "the rendered image is too wide".to_string())?;
        let height = u32::try_from(exported.height).map_err(|_| "the rendered image is too tall".to_string())?;
        (exported.bytes, width, height)
    } else {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        (bytes, ph.width, ph.height)
    };

    let mut packet_photo = (*ph).clone();
    if render {
        packet_photo.develop = Arc::new(neutral.clone());
        packet_photo.edited = None;
        packet_photo.history.clear();
        packet_photo.history_cursor = None;
    }
    let fresh = file_packet(&packet_photo, &s.catalog);
    let owned: Vec<&str> = OWNED.iter().chain(FILE_OWNED.iter()).copied().collect();
    let rules = MergeRules { owned: &owned, owned_if_present: OWNED_IF_STATED };
    let packet = match lightcraft_meta::embedded(&bytes).xmp {
        Some(old) => lightcraft_meta::merge_xmp(&old, &fresh, rules).map_err(|e| format!("the file's XMP can't be merged: {e}"))?,
        None => fresh,
    };
    let out = lightcraft_meta::set_xmp(&bytes, &packet).map_err(str::to_string)?;
    let out = lightcraft_meta::set_iptc_keywords(&out, &flat_tags(&packet_photo.meta.keywords)).map_err(str::to_string)?;

    lightcraft_catalog::safe_file::write_atomic(path, &out).map_err(|e| format!("write {}: {e}", path.display()))?;

    let content_hash = Some(lightcraft_preview::hash_bytes(&out).to_string());
    let file_size = u64::try_from(out.len()).map_err(|_| "the file is too large".to_string())?;
    let (preview_only, preview_checked) = (ph.preview_only.clone(), ph.preview_checked.clone());
    s.apply_system(Op::SetContent { id, width, height, file_size, content_hash, preview_only, preview_checked }).map_err(|e| e.to_string())?;
    if render {
        let label = "Bake edits into file".to_string();
        s.apply_system(Op::SetDevelop { id, settings: Arc::new(neutral), label, edited: None }).map_err(|e| e.to_string())?;
        s.apply_system(Op::SetHistory { id, history: Vec::new() }).map_err(|e| e.to_string())?;
        s.apply_system(Op::SetHistoryCursor { id, cursor: None }).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_catalog::{Photo, Source};
    use serde_json::{Value, json};
    use std::path::PathBuf;

    #[test]
    fn only_jpeg_and_png_are_bakeable() {
        assert!(is_bakeable("/x/a.JPG"));
        assert!(is_bakeable("/x/a.jpeg"));
        assert!(is_bakeable("/x/a.png"));
        assert!(!is_bakeable("/x/a.cr3"));
        assert!(!is_bakeable("/x/a.tif"));
        assert!(!is_bakeable("/x/png"));
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lc-bake-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A session holding one photo (id 1) whose file is `path`.
    fn session_with_file(path: &Path, name: &str, format: &str, w: u32, h: u32) -> Session {
        let mut s = Session::new().with_fs();
        let source = Source::File { path: path.to_string_lossy().to_string() };
        let photo = Photo::new(PhotoId(1), source, name, format, w, h, "2026-03-15T19:42:08");
        s.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        s.execute("library.select", &json!({"ids": [1]})).unwrap();
        s
    }

    /// Sets the target folder and stages photo 1 into `sub`, then runs "Move Images".
    fn move_to(s: &mut Session, target: &Path, sub: &str) -> Value {
        s.execute("organizer.setTargetFolder", &json!({"path": target.to_string_lossy()})).unwrap();
        s.execute("organizer.stageSubfolder", &json!({"subfolder": sub})).unwrap();
        s.execute("organizer.moveImages", &json!({})).unwrap()
    }

    /// A real JPEG rendered from the demo library.
    fn demo_jpeg() -> (Vec<u8>, u32, u32) {
        let mut demo = Session::with_demo();
        let id = demo.active().unwrap();
        let exported = crate::export::export_photo(&mut demo, id, &crate::export::ExportOptions::default(), 0).unwrap();
        (exported.bytes, u32::try_from(exported.width).unwrap(), u32::try_from(exported.height).unwrap())
    }

    #[test]
    fn moved_jpeg_gets_its_tags_into_the_file_and_no_sidecar() {
        let dir = scratch("tags");
        let (bytes, w, h) = demo_jpeg();
        let src = dir.join("shot.jpg");
        std::fs::write(&src, &bytes).unwrap();
        let mut s = session_with_file(&src, "shot.jpg", "JPG", w, h);
        s.execute("photo.setMeta", &json!({"title": "Fresh", "addKeywords": ["café"]})).unwrap();

        let moved = move_to(&mut s, &dir.join("Fotos"), "2026");

        assert_eq!(moved["imagesWritten"], 1, "{moved}");
        assert!(moved["imageFailed"].as_array().unwrap().is_empty(), "{moved}");
        assert!(!dir.join("Fotos/2026/shot.xmp").exists());
        let baked = std::fs::read(dir.join("Fotos/2026/shot.jpg")).unwrap();
        let xmp = lightcraft_meta::embedded(&baked).xmp.unwrap();
        assert!(xmp.contains("Fresh") && xmp.contains("café"), "{xmp}");
    }

    #[test]
    fn moved_jpeg_gets_the_media_organizer_tag_fields() {
        let dir = scratch("fields");
        let (bytes, w, h) = demo_jpeg();
        let src = dir.join("shot.jpg");
        std::fs::write(&src, &bytes).unwrap();
        let mut s = session_with_file(&src, "shot.jpg", "JPG", w, h);
        s.execute("photo.setMeta", &json!({"addKeywords": ["Reisen|Lissabon", "Essen"]})).unwrap();

        let moved = move_to(&mut s, &dir.join("Fotos"), "2026");

        assert_eq!(moved["imagesWritten"], 1, "{moved}");
        let baked = std::fs::read(dir.join("Fotos/2026/shot.jpg")).unwrap();
        let embedded = lightcraft_meta::embedded(&baked);
        let xmp = embedded.xmp.clone().unwrap();
        assert!(xmp.contains("digiKam:TagsList"), "{xmp}");
        let read = lightcraft_meta::parse_xmp(&xmp).unwrap().metadata;
        assert_eq!(read.keywords, vec!["Lissabon", "Essen"]);
        assert_eq!(read.hierarchical_keywords, vec!["Reisen|Lissabon", "Essen"]);
        let iptc = lightcraft_meta::parse_iptc(&embedded.iptc.unwrap());
        assert_eq!(iptc.keywords, vec!["Lissabon", "Essen"]);
    }

    #[test]
    fn moved_png_gets_its_tags_in_xmp() {
        let dir = scratch("png");
        let mut demo = Session::with_demo();
        let id = demo.active().unwrap();
        let opts = crate::export::ExportOptions { format: ExportFormat::Png, ..Default::default() };
        let exported = export_photo(&mut demo, id, &opts, 0).unwrap();
        let src = dir.join("shot.png");
        std::fs::write(&src, &exported.bytes).unwrap();
        let mut s = session_with_file(&src, "shot.png", "PNG", u32::try_from(exported.width).unwrap(), u32::try_from(exported.height).unwrap());
        s.execute("photo.setMeta", &json!({"addKeywords": ["Reisen|Lissabon"]})).unwrap();

        let moved = move_to(&mut s, &dir.join("Fotos"), "2026");

        assert_eq!(moved["imagesWritten"], 1, "{moved}");
        let baked = std::fs::read(dir.join("Fotos/2026/shot.png")).unwrap();
        let xmp = lightcraft_meta::embedded(&baked).xmp.unwrap();
        let read = lightcraft_meta::parse_xmp(&xmp).unwrap().metadata;
        assert_eq!(read.keywords, vec!["Lissabon"]);
        assert_eq!(read.hierarchical_keywords, vec!["Reisen|Lissabon"]);
    }

    #[test]
    fn moved_jpeg_with_edits_is_rendered_into_the_file_and_the_edits_are_forgotten() {
        let dir = scratch("edit");
        let (bytes, w, h) = demo_jpeg();
        let src = dir.join("shot.jpg");
        std::fs::write(&src, &bytes).unwrap();
        let mut s = session_with_file(&src, "shot.jpg", "JPG", w, h);
        s.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();

        let moved = move_to(&mut s, &dir.join("Fotos"), "2026");

        assert_eq!(moved["imagesWritten"], 1, "{moved}");
        assert!(moved["imageFailed"].as_array().unwrap().is_empty(), "{moved}");
        let baked = std::fs::read(dir.join("Fotos/2026/shot.jpg")).unwrap();
        assert_ne!(baked, bytes, "the edit must change the pixels");
        let ph = s.catalog.photo(PhotoId(1)).unwrap();
        assert_eq!(*ph.develop, ph.camera_defaults());
        assert!(ph.edited.is_none());
        assert_eq!(ph.file_size, u64::try_from(baked.len()).unwrap());
    }

    #[test]
    fn a_file_shared_by_virtual_copies_is_moved_untouched_and_reported() {
        let dir = scratch("copies");
        let src = dir.join("shot.jpg");
        std::fs::write(&src, b"not really a jpeg").unwrap();
        let mut s = session_with_file(&src, "shot.jpg", "JPG", 6000, 4000);
        let copy =
            Photo::new(PhotoId(2), Source::File { path: src.to_string_lossy().to_string() }, "shot.jpg", "JPG", 6000, 4000, "2026-03-15T19:42:08");
        s.catalog.apply(Op::AddPhoto { photo: Box::new(copy) }).unwrap();

        let moved = move_to(&mut s, &dir.join("Fotos"), "2026");

        assert_eq!(moved["imagesWritten"], 0, "{moved}");
        let failed = moved["imageFailed"].as_array().unwrap();
        assert_eq!(failed.len(), 1, "{moved}");
        assert!(failed[0]["error"].as_str().unwrap().contains("virtual copies"), "{moved}");
        assert_eq!(std::fs::read(dir.join("Fotos/2026/shot.jpg")).unwrap(), b"not really a jpeg");
    }

    #[test]
    fn moved_raw_photo_still_gets_a_sidecar() {
        let dir = scratch("raw");
        let src = dir.join("shot.cr3");
        std::fs::write(&src, b"not really raw").unwrap();
        let mut s = session_with_file(&src, "shot.cr3", "CR3", 6000, 4000);
        s.execute("photo.rate", &json!({"rating": 3})).unwrap();

        let moved = move_to(&mut s, &dir.join("Fotos"), "2026");

        assert_eq!(moved["imagesWritten"], 0, "{moved}");
        assert!(dir.join("Fotos/2026/shot.xmp").exists());
    }
}
