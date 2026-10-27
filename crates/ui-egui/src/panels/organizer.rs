//! Media Organizer UI:
//! - Organizer Bar above the grid (Todo-folder switcher, Target Folder selector, Assign Subfolder (M),
//!   Move Images, Show Subfolders toggle, Quick Tags (T), Crop 16:9 / 24:10 + Preview, Screenshot Converter,
//!   Add PiGallery2 URL to MyStuff).
//! - Staged subfolder badge on grid thumbnails and in Detail / Fullscreen (`F`) view.
//! - Modal dialogs: `QuickMove` (`M`), `QuickTags` (`T`), `CropPreview`, `ConvertScreenshots`, `MyStuffPigallery`.

use std::path::Path;

use egui::{Align2, Color32, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use lightcraft_catalog::PhotoId;
use lightcraft_engine::cmd::organizer::{
    MyStuffGameEntry, MyStuffMediaEntry, ScreenshotPattern, current_source_folder, is_folder_empty, matched_target_root, pigallery_url_for_path,
    sync_target_folder_for_source,
};
use serde_json::{Value, json};

use crate::LightcraftApp;
use crate::state::Dialog;
use crate::theme::Tokens;
use crate::widgets::{register, text_button};

/// Whether the Organizer Bar should be shown above the grid.
pub fn should_show_bar(app: &LightcraftApp) -> bool {
    if app.ui.organizer_bar || !app.session.organizer.staged_moves.is_empty() {
        return true;
    }
    let src = current_source_folder(&app.session);
    matched_target_root(&app.session, src.as_deref()).is_some() || src.as_deref().map(Path::new).is_some_and(is_folder_empty)
}

fn short_folder_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    Path::new(trimmed).file_name().map(|n| n.to_string_lossy().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| trimmed.to_string())
}

/// Open the `QuickMove` (`M`) dialog initialized for the current selection.
pub fn open_quick_move(app: &mut LightcraftApp) {
    sync_target_folder_for_source(&mut app.session);
    let current = app
        .session
        .active()
        .and_then(|id| app.session.organizer.staged_moves.get(&id))
        .cloned()
        .or_else(|| app.session.organizer.recent_subfolders.first().cloned())
        .unwrap_or_default();
    app.ui.dialog = Some(Dialog::QuickMove { subfolder: current, cursor: 0, fresh: true });
}

/// Open the `QuickTags` (`T`) dialog initialized for the current selection.
pub fn open_quick_tags(app: &mut LightcraftApp) {
    app.ui.dialog = Some(Dialog::QuickTags { text: String::new(), cursor: 0, fresh: true });
}

/// Open the `CropPreview` dialog for the current selection.
pub fn open_crop_preview(app: &mut LightcraftApp, aspect: &str) {
    let aspect = if aspect.is_empty() { "16x9" } else { aspect };
    app.ui.dialog = Some(Dialog::CropPreview { aspect: aspect.to_string(), index: 0 });
}

/// Open the `ConvertScreenshots` dialog initialized from `organizer.detectScreenshots`.
pub fn open_convert_screenshots(app: &mut LightcraftApp) {
    let all_visible = app.session.selection.ids.len() <= 1;
    let detected = app.session.execute("organizer.detectScreenshots", &json!({ "allVisible": all_visible })).unwrap_or_else(|_| json!({}));
    let from_pattern = detected.get("dominantFrom").and_then(Value::as_str).unwrap_or("SHAREX_SPECTACLE").to_string();
    let to_pattern = if from_pattern == "SHAREX_SPECTACLE" { "STEAM".to_string() } else { "SHAREX_SPECTACLE".to_string() };
    app.ui.dialog = Some(Dialog::ConvertScreenshots { from_pattern, to_pattern, hide_unrenamed: true, all_visible });
}

/// Open the `MyStuffPigallery` dialog for the current target or source folder.
pub fn open_mystuff_pigallery(app: &mut LightcraftApp) {
    sync_target_folder_for_source(&mut app.session);
    let src = current_source_folder(&app.session);

    // Find the destination subfolder of the images in the current folder / view
    let staged_sub = {
        let active_id = app.session.active();
        let active_sub = active_id.and_then(|id| app.session.organizer.staged_moves.get(&id)).cloned();
        if active_sub.is_some() {
            active_sub
        } else {
            let sel = app.session.selected();
            let sel_sub = sel.iter().find_map(|id| app.session.organizer.staged_moves.get(id)).cloned();
            if sel_sub.is_some() {
                sel_sub
            } else {
                app.session.visible_cloned().iter().find_map(|id| app.session.organizer.staged_moves.get(id)).cloned()
            }
        }
    };

    let target_base = app
        .session
        .organizer
        .target_folder
        .clone()
        .or_else(|| src.as_deref().and_then(|s| matched_target_root(&app.session, Some(s))))
        .or_else(|| app.session.organizer.mappings.first().map(|m| m.target_root.clone()));

    let folder = match (target_base, staged_sub.as_deref()) {
        (Some(base), Some(sub)) if !sub.is_empty() && sub != "." => Some(Path::new(&base).join(sub).to_string_lossy().to_string()),
        (Some(base), _) => Some(base),
        (None, _) => src,
    };

    let url = pigallery_url_for_path(&app.session, folder.as_deref()).unwrap_or_default();
    let host = app.session.organizer.mystuff_host.clone();

    // Try to guess a game name for initial selection from the staged subfolder or folder name
    let guess = staged_sub.as_deref().map(short_folder_name).or_else(|| folder.as_deref().map(short_folder_name)).unwrap_or_default();

    let mut dlg = Dialog::MyStuffPigallery {
        host,
        pigallery_url: url,
        games: Vec::new(),
        selected_game: 0,
        game_filter: String::new(),
        media: Vec::new(),
        error: None,
        loaded_host: None,
    };
    reload_mystuff_host(app, &mut dlg, Some(&guess));
    app.ui.dialog = Some(dlg);
}

fn reload_mystuff_host(app: &mut LightcraftApp, dlg: &mut Dialog, guess: Option<&str>) {
    let Dialog::MyStuffPigallery { host, pigallery_url, games, selected_game, game_filter, media, error, loaded_host } = dlg else {
        return;
    };
    *loaded_host = Some(host.clone());
    match app.session.execute("organizer.mystuffLoadGames", &json!({ "host": host.clone() })) {
        Ok(res) => {
            let parsed: Vec<MyStuffGameEntry> = res.get("games").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
            *games = parsed;
            *error = None;
            // Pick best match for `game_filter` or `guess` if non-empty
            let filter = if !game_filter.trim().is_empty() { game_filter.as_str() } else { guess.unwrap_or("") };
            let f_lower = filter.trim().to_lowercase();
            let idx = if !f_lower.is_empty() {
                games
                    .iter()
                    .position(|g| g.name.to_lowercase() == f_lower)
                    .or_else(|| games.iter().position(|g| g.name.to_lowercase().contains(&f_lower)))
                    .unwrap_or(0)
            } else {
                0
            };
            *selected_game = idx;
            *media = games.get(idx).map(|g| g.media.clone()).unwrap_or_default();
            if media.is_empty() && !pigallery_url.is_empty() {
                media.push(MyStuffMediaEntry { name: "Screenshots".into(), location: pigallery_url.clone() });
            }
        }
        Err(e) => {
            *games = Vec::new();
            *selected_game = 0;
            *media = Vec::new();
            *error = Some(e.to_string());
        }
    }
}

/// Render the Media Organizer toolbar above the photo grid.
pub fn bar(app: &mut LightcraftApp, ui: &mut egui::Ui) {
    sync_target_folder_for_source(&mut app.session);
    let t = Tokens::get(ui.ctx());
    let src_folder = current_source_folder(&app.session);
    let matched_root = matched_target_root(&app.session, src_folder.as_deref());
    let target_folder = app.session.organizer.target_folder.clone().unwrap_or_default();
    let staged_count = app.session.organizer.staged_moves.keys().filter(|id| app.session.catalog.photo(**id).is_some_and(|p| !p.deleted)).count();
    let is_empty = app.session.visible().is_empty() && src_folder.as_deref().map(Path::new).is_some_and(is_folder_empty);

    egui::Frame::NONE.fill(t.chrome).stroke(Stroke::new(1.0, t.divider)).inner_margin(egui::Margin::symmetric(14, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Row 1: Todo source switcher + Target Root & Target Folder selector + Subfolder actions + Move Images
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(egui::RichText::new("Organizer").font(t.semibold(12.0)).color(t.accent));

            // Quick Todo Folder switcher dropdown
            let src_label = src_folder.as_deref().map(short_folder_name).unwrap_or_else(|| "Todo Folder…".into());
            let todo_drop = crate::widgets::dropdown(ui, "organizerTodoSource", &format!("Source: {src_label}"), t.font(12.0), t.text_label)
                .on_hover_text("Switch between configured Todo folders (_todo_photos, _todo_media, _videos_to_sort)");
            egui::Popup::menu(&todo_drop).show(|ui| {
                let mappings = app.session.organizer.mappings.clone();
                for m in mappings {
                    let name = short_folder_name(&m.source);
                    let exists = Path::new(&m.source).is_dir();
                    let label = format!("{name}  →  {}", short_folder_name(&m.target_root));
                    let btn = ui.add_enabled(exists, egui::Button::new(label));
                    register(ui, format!("organizerTodo:{}", m.source), btn.rect);
                    if btn.clicked() {
                        if let Err(e) = app.run("library.browse", json!({"path": m.source})) {
                            app.toast(ui.ctx(), e);
                        }
                        ui.close();
                    }
                }
                if app.services.pick_folder.is_some() {
                    ui.separator();
                    if ui.button("Browse Other Source Folder…").clicked() {
                        if let Some(picked) = app.services.pick_folder.as_mut().and_then(|f| f()) {
                            let _ = app.run("library.browse", json!({"path": picked}));
                        }
                        ui.close();
                    }
                }
            });

            ui.label(egui::RichText::new("→").color(t.text_dim));

            // Target Folder selector (lists target roots and child folders like 2026, 2025, etc.)
            let tf_display = if target_folder.is_empty() {
                "Select Target Folder…".to_string()
            } else {
                let parent_name =
                    Path::new(&target_folder).parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let leaf = short_folder_name(&target_folder);
                if parent_name.is_empty() || parent_name == leaf { leaf } else { format!("{parent_name}/{leaf}") }
            };
            let tf_drop = crate::widgets::dropdown(ui, "organizerTargetFolder", &format!("Target: {tf_display}"), t.font(12.0), t.text)
                .on_hover_text(if target_folder.is_empty() { "Select destination root folder" } else { &target_folder });
            egui::Popup::menu(&tf_drop).show(|ui| {
                let mut roots: Vec<String> = Vec::new();
                if let Some(mr) = &matched_root {
                    roots.push(mr.clone());
                }
                for m in &app.session.organizer.mappings {
                    let tr = m.target_root.trim_end_matches(['/', '\\']).to_string();
                    if !roots.contains(&tr) {
                        roots.push(tr);
                    }
                }
                for root in roots {
                    let root_name = short_folder_name(&root);
                    ui.label(egui::RichText::new(format!("{root_name} ({root})")).size(11.0).color(t.text_dim));
                    for cand in app.session.organizer.target_folders_for(&root) {
                        let is_root = cand == root;
                        let lbl = if is_root { format!("📁 {root_name} (Root)") } else { format!("  ↳ {}", short_folder_name(&cand)) };
                        let sel = cand == target_folder;
                        let item = ui.selectable_label(sel, lbl);
                        register(ui, format!("organizerTargetOption:{cand}"), item.rect);
                        if item.clicked() {
                            let _ = app.run("organizer.setTargetFolder", json!({"path": cand}));
                            ui.close();
                        }
                    }
                    ui.separator();
                }
                if ui.button("↻ Refresh Folders").clicked() {
                    app.session.organizer.invalidate_folder_cache();
                }
                if app.services.pick_folder.is_some() && ui.button("Choose Target Folder on Disk…").clicked() {
                    if let Some(picked) = app.services.pick_folder.as_mut().and_then(|f| f()) {
                        let _ = app.run("organizer.setTargetFolder", json!({"path": picked}));
                    }
                    ui.close();
                }
            });

            // Assign Subfolder (M) button
            let assign_btn = text_button(ui, "organizerAssignSubfolder", "Assign Subfolder (M)", false)
                .on_hover_text("Assign destination subfolder to selected photo(s) (Shortcut: M)");
            if assign_btn.clicked() {
                open_quick_move(app);
            }

            // Move Images (Ctrl+Alt+M) button
            let move_label = if staged_count > 0 { format!("Move Images ({staged_count})") } else { "Move Images".to_string() };
            let move_btn = ui
                .add_enabled(
                    staged_count > 0,
                    egui::Button::new(egui::RichText::new(move_label).font(t.semibold(12.0)).color(if staged_count > 0 {
                        Color32::WHITE
                    } else {
                        t.text_dim
                    }))
                    .fill(if staged_count > 0 { t.accent } else { t.cell }),
                )
                .on_hover_text("Move all photos with an assigned subfolder into the target folder (Shortcut: Ctrl+Alt+M; Undoable with Ctrl+Z)");
            register(ui, "button:organizerMoveImages", move_btn.rect);
            if move_btn.clicked() {
                match app.run("organizer.moveImages", json!({})) {
                    Ok(r) => {
                        let n = r.get("moved").and_then(Value::as_u64).unwrap_or(0);
                        app.toast(ui.ctx(), format!("Moved {n} photo{} to target folder", if n == 1 { "" } else { "s" }));
                    }
                    Err(e) => app.toast_error(ui.ctx(), e),
                }
            }

            if staged_count > 0
                && text_button(ui, "organizerClearStaged", "Clear", false).on_hover_text("Clear staged subfolder assignments").clicked()
            {
                let _ = app.run("organizer.clearStaged", json!({}));
            }

            if is_empty && let Some(src_path) = &src_folder {
                let del_btn = ui
                    .add(egui::Button::new(egui::RichText::new("Delete Folder").font(t.semibold(12.0)).color(Color32::WHITE)).fill(t.reject))
                    .on_hover_text(format!("Delete empty folder '{}' from disk", short_folder_name(src_path)));
                register(ui, "button:organizerDeleteFolder", del_btn.rect);
                if del_btn.clicked() {
                    let folder_name = short_folder_name(src_path);
                    match app.run("organizer.deleteFolder", json!({"path": src_path})) {
                        Ok(_) => {
                            app.toast(ui.ctx(), format!("Deleted empty folder '{folder_name}'"));
                        }
                        Err(e) => app.toast_error(ui.ctx(), e),
                    }
                }
            }

            // Show subfolders on images checkbox
            let mut show_sub = app.session.organizer.show_subfolders_on_images;
            let chk = ui.checkbox(&mut show_sub, "Show subfolders");
            register(ui, "check:organizerShowSubfolders", chk.rect);
            if chk.changed() {
                let _ = app.run("organizer.setShowSubfolders", json!({"show": show_sub}));
            }
        });

        ui.add_space(4.0);

        // Row 2: Quick Tags (T), Batch Crop 16:9 / 24:10 + Preview, Convert Screenshot Names, PiGallery2 / MyStuff
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;

            if text_button(ui, "organizerQuickTags", "Quick Tags (T)", false)
                .on_hover_text("Rapidly add or remove multiple tags for the selected photo(s) (Shortcut: T)")
                .clicked()
            {
                open_quick_tags(app);
            }

            ui.separator();

            ui.label(egui::RichText::new("Crop:").size(11.5).color(t.text_dim));
            if text_button(ui, "organizerCrop16x9", "16:9", false).on_hover_text("Crop all selected photos to 16:9 aspect ratio (Undoable)").clicked()
            {
                match app.run("crop.aspect", json!({"aspect": "16x9", "selection": true})) {
                    Ok(_) => app.toast(ui.ctx(), "Cropped selected photo(s) to 16:9"),
                    Err(e) => app.toast_error(ui.ctx(), e),
                }
            }
            if text_button(ui, "organizerCrop24x10", "24:10", false)
                .on_hover_text("Crop all selected photos to 24:10 aspect ratio (Undoable)")
                .clicked()
            {
                match app.run("crop.aspect", json!({"aspect": "24x10", "selection": true})) {
                    Ok(_) => app.toast(ui.ctx(), "Cropped selected photo(s) to 24:10"),
                    Err(e) => app.toast_error(ui.ctx(), e),
                }
            }
            if text_button(ui, "organizerCropPreview", "Crop Preview…", false)
                .on_hover_text("Preview 16:9 or 24:10 crop on the selected photo(s) before applying")
                .clicked()
            {
                open_crop_preview(app, "16x9");
            }

            ui.separator();

            if text_button(ui, "organizerScreenshots", "Convert Screenshots…", false)
                .on_hover_text("Detect and convert screenshot filenames (ShareX/Spectacle, Steam, NVIDIA, AMD, Camera)")
                .clicked()
            {
                open_convert_screenshots(app);
            }

            let pg_folder = if !target_folder.is_empty() { Some(target_folder.as_str()) } else { src_folder.as_deref() };
            let pg_url = pigallery_url_for_path(&app.session, pg_folder);
            let pg_hover = pg_url
                .as_deref()
                .map(|u| format!("Add PiGallery2 URL to MyStuff MongoDB:\n{u}"))
                .unwrap_or_else(|| "Add PiGallery2 URL to MyStuff MongoDB".to_string());
            if text_button(ui, "organizerMyStuff", "Add PiGallery2 URL to MyStuff…", false).on_hover_text(pg_hover).clicked() {
                open_mystuff_pigallery(app);
            }
        });
    });
}

/// Draw the staged destination subfolder badge on a grid thumbnail cell (`img_rect`).
pub fn cell_subfolder_badge(app: &mut LightcraftApp, ui: &mut egui::Ui, id: PhotoId, img_rect: Rect, square: bool) {
    if !app.session.organizer.show_subfolders_on_images {
        return;
    }
    let Some(sub) = app.session.organizer.staged_moves.get(&id).cloned() else { return };
    let t = Tokens::get(ui.ctx());
    let p = ui.painter();
    let label = if sub == "." { "→ [Root]".to_string() } else { format!("→ {sub}") };
    let max_chars = ((img_rect.width() - 28.0) / 6.5).max(8.0) as usize;
    let shown = if label.chars().count() > max_chars {
        let tail: String = label.chars().rev().take(max_chars.saturating_sub(2)).collect::<Vec<_>>().into_iter().rev().collect();
        format!("→ …{tail}")
    } else {
        label.clone()
    };
    let g = p.layout_no_wrap(shown, t.semibold(10.5), Color32::WHITE);
    let y_top = if square { img_rect.top() + 6.0 } else { img_rect.top() + 5.0 };
    let x_right = img_rect.right() - 6.0;
    let w = (g.size().x + 14.0).min(img_rect.width() - 12.0);
    let br = Rect::from_min_size(pos2(x_right - w, y_top), vec2(w, 20.0));
    p.rect_filled(br, 6.0, t.accent.gamma_multiply(0.92));
    p.rect_stroke(br, 6.0, Stroke::new(1.0, Color32::from_white_alpha(90)), StrokeKind::Inside);
    p.galley(pos2(br.left() + 7.0, br.center().y - g.size().y / 2.0), g, Color32::WHITE);
    register(ui, format!("badge:subfolder:{}", id.0), br);
    let resp = ui
        .interact(br, egui::Id::unique(("subfolder-badge", id.0)), Sense::click())
        .on_hover_text(format!("Staged destination subfolder: {sub}\nClick or press M to change"));
    if resp.clicked() {
        if !app.session.selection.contains(id) {
            let _ = app.run("library.select", json!({"ids": [id.0]}));
        }
        open_quick_move(app);
    }
}

/// Draw the staged destination subfolder badge in the Detail / Fullscreen (`F`) view.
pub fn loupe_subfolder_badge(app: &mut LightcraftApp, ui: &mut egui::Ui, id: PhotoId, canvas: Rect) {
    if !app.session.organizer.show_subfolders_on_images || app.ui.fullscreen {
        return;
    }
    let Some(sub) = app.session.organizer.staged_moves.get(&id).cloned() else { return };
    let t = Tokens::get(ui.ctx());
    let p = ui.painter();
    let label = if sub == "." { "Move to: [Target Root] (M)".to_string() } else { format!("Move to: {sub} (M)") };
    let g = p.layout_no_wrap(label, t.semibold(12.5), Color32::WHITE);
    let br = Rect::from_min_size(pos2(canvas.right() - g.size().x - 32.0, canvas.top() + 14.0), vec2(g.size().x + 20.0, 26.0));
    p.rect_filled(br, 6.0, t.accent.gamma_multiply(0.92));
    p.galley(pos2(br.left() + 10.0, br.center().y - g.size().y / 2.0), g, Color32::WHITE);
    register(ui, "loupe:subfolderBadge", br);
    let resp = ui.interact(br, egui::Id::unique("loupe-subfolder-badge"), Sense::click());
    if resp.clicked() {
        open_quick_move(app);
    }
}

// ---------- Modal Dialogs ----------

pub fn show_quick_move_dialog(
    app: &mut LightcraftApp,
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    subfolder: &mut String,
    cursor: &mut usize,
    fresh: &mut bool,
    close: &mut bool,
) {
    use std::hash::{Hash, Hasher};

    let t = Tokens::get(ctx);
    let sel_count = app.session.selected().len();
    let target_folder = app.session.organizer.target_folder.clone().unwrap_or_default();

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Target Folder:").color(t.text_dim));
        if target_folder.is_empty() {
            ui.label(egui::RichText::new("None selected").color(t.caution));
        } else {
            ui.label(egui::RichText::new(&target_folder).font(t.semibold(12.0)).color(t.text));
        }
        let reload_btn = ui.button("↻ Reload").on_hover_text("Rescan subfolders on disk");
        register(ui, "button:quickMoveReload", reload_btn.rect);
        if reload_btn.clicked() {
            app.session.organizer.invalidate_folder_cache();
            ctx.data_mut(|d| d.remove_temp::<(u64, Vec<String>)>(egui::Id::unique("quick-move-filtered-matches")));
            app.toast(ctx, "Reloaded subfolders from disk");
        }
    });
    ui.label(
        egui::RichText::new(format!(
            "Assigning destination subfolder for {sel_count} selected photo{} (Tab = Complete, Enter = Assign, Ctrl+Enter = Assign & Move Now)",
            if sel_count == 1 { "" } else { "s" }
        ))
        .size(11.5)
        .color(t.text_dim),
    );
    ui.add_space(6.0);

    // Gather available subfolders from the in-memory session cache (zero disk I/O per frame),
    // and cache the filtered `matches` list by `(target_folder, query, subfolder_count, recent_subfolders)`
    // so filtering only runs when the query text or folder list changes.
    let q = subfolder.trim().to_lowercase();
    let recent = app.session.organizer.recent_subfolders.clone();
    let all_subs = app.session.organizer.subfolders_for(&target_folder);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&target_folder, &q, all_subs, &recent).hash(&mut hasher);
    let filter_key = hasher.finish();
    let cache_id = egui::Id::unique("quick-move-filtered-matches");

    let matches: Vec<String> = match ctx.data(|d| d.get_temp::<(u64, Vec<String>)>(cache_id)) {
        Some((k, cached)) if k == filter_key => cached,
        _ => {
            let mut out = Vec::with_capacity(all_subs.len().min(128));
            if q.is_empty() {
                let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::with_capacity(all_subs.len());
                for r in &recent {
                    if all_subs.binary_search(r).is_ok() && seen.insert(r.as_str()) {
                        out.push(r.clone());
                    }
                }
                for s in all_subs {
                    if seen.insert(s.as_str()) {
                        out.push(s.clone());
                    }
                }
            } else {
                // Single-pass O(N) partition: prefix matches first, then substring matches
                let mut sub_matches = Vec::new();
                for s in all_subs {
                    let sl = s.to_lowercase();
                    if sl.starts_with(&q) || sl.split('/').any(|part| part.starts_with(&q)) {
                        out.push(s.clone());
                    } else if sl.contains(&q) {
                        sub_matches.push(s.clone());
                    }
                }
                out.extend(sub_matches);
            }
            ctx.data_mut(|d| d.insert_temp(cache_id, (filter_key, out.clone())));
            out
        }
    };

    if *cursor >= matches.len() {
        *cursor = 0;
    }

    // Handle Up/Down/Tab/Enter/Ctrl+Enter before TextEdit consumes them
    let mut trigger_assign = false;
    let mut trigger_move_now = false;
    let mut trigger_create = false;
    let mut trigger_tab_complete = false;
    let mut scroll_to_cursor = *fresh;

    ui.input_mut(|i| {
        if i.consume_key(Modifiers::NONE, Key::ArrowDown) && !matches.is_empty() {
            *cursor = (*cursor + 1).min(matches.len().saturating_sub(1));
            scroll_to_cursor = true;
        } else if i.consume_key(Modifiers::NONE, Key::ArrowUp) && !matches.is_empty() {
            *cursor = cursor.saturating_sub(1);
            scroll_to_cursor = true;
        } else if i.consume_key(Modifiers::NONE, Key::Tab) && !matches.is_empty() {
            trigger_tab_complete = true;
        } else if i.consume_key(Modifiers::COMMAND, Key::Enter) || i.consume_key(Modifiers::CTRL, Key::Enter) {
            trigger_move_now = true;
        } else if i.consume_key(Modifiers::NONE, Key::Enter) {
            trigger_assign = true;
        }
    });

    if trigger_tab_complete && let Some(chosen) = matches.get(*cursor) {
        *subfolder = chosen.clone();
        scroll_to_cursor = true;
    }

    let prev_text = subfolder.clone();
    let edit_resp = ui.add(
        egui::TextEdit::singleline(subfolder).hint_text("Type subfolder (e.g. _Lea/alleine or _Flauschis/Chillen, . for root)…").desired_width(480.0),
    );
    register(ui, "field:quickMoveSubfolder", edit_resp.rect);
    if *fresh || trigger_tab_complete {
        edit_resp.request_focus();
        *fresh = false;
    }
    if *subfolder != prev_text {
        *cursor = 0;
        scroll_to_cursor = true;
    }

    ui.add_space(6.0);
    egui::ScrollArea::vertical().id_salt("quick-move-list").max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.set_min_width(460.0);
        if matches.is_empty() {
            ui.label(
                egui::RichText::new("No existing subfolder matches — press 'Create & Assign' to create a new subfolder.")
                    .size(12.0)
                    .color(t.text_dim),
            );
        } else {
            for (idx, item) in matches.iter().take(60).enumerate() {
                let is_cur = idx == *cursor;
                let display = if item == "." { "📁 . (Target Folder Root)".to_string() } else { format!("📁 {item}") };
                let resp = ui.selectable_label(is_cur, display);
                register(ui, format!("quickMoveOption:{item}"), resp.rect);
                if is_cur && scroll_to_cursor {
                    resp.scroll_to_me(Some(egui::Align::Center));
                }
                if resp.clicked() {
                    *subfolder = item.clone();
                    trigger_assign = true;
                }
            }
        }
    });

    let typed_clean = subfolder.trim().trim_matches('/').to_string();
    let exact_exists = matches.iter().any(|m| m.eq_ignore_ascii_case(&typed_clean));

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        if ui.button("Clear Assignment").clicked() {
            let _ = app.run("organizer.stageSubfolder", json!({"subfolder": null}));
            *close = true;
        }
        if !typed_clean.is_empty() && typed_clean != "." && !exact_exists {
            let create_btn = ui.button(format!("+ Create & Assign “{typed_clean}”"));
            register(ui, "button:quickMoveCreate", create_btn.rect);
            if create_btn.clicked() {
                trigger_create = true;
            }
        }
        let assign_btn = ui.button(egui::RichText::new("Assign (Enter)").color(t.accent));
        register(ui, "button:quickMoveAssign", assign_btn.rect);
        if assign_btn.clicked() {
            trigger_assign = true;
        }
        let move_now_btn = ui.button("Assign & Move Now (Ctrl+Enter)");
        register(ui, "button:quickMoveNow", move_now_btn.rect);
        if move_now_btn.clicked() {
            trigger_move_now = true;
        }
    });

    let chosen_sub = || -> String {
        if let Some(m) = matches.get(*cursor) {
            m.clone()
        } else if typed_clean.is_empty() {
            ".".to_string()
        } else {
            typed_clean.clone()
        }
    };

    if trigger_create {
        match app.run("organizer.createSubfolder", json!({"subfolder": typed_clean, "assignToSelection": true})) {
            Ok(_) => {
                app.toast(ctx, format!("Created and assigned subfolder “{typed_clean}”"));
                *close = true;
            }
            Err(e) => app.toast_error(ctx, e),
        }
    } else if trigger_assign || trigger_move_now {
        let chosen = chosen_sub();
        // If the user typed a brand-new subfolder path that had no matches, create it automatically
        let res = if matches.is_empty() && chosen != "." && !chosen.is_empty() {
            app.run("organizer.createSubfolder", json!({"subfolder": chosen, "assignToSelection": true}))
        } else {
            app.run("organizer.stageSubfolder", json!({"subfolder": chosen}))
        };
        match res {
            Ok(_) => {
                if trigger_move_now {
                    match app.run("organizer.moveImages", json!({})) {
                        Ok(r) => {
                            let n = r.get("moved").and_then(Value::as_u64).unwrap_or(0);
                            app.toast(ctx, format!("Moved {n} photo{} to {chosen}", if n == 1 { "" } else { "s" }));
                        }
                        Err(e) => app.toast_error(ctx, e),
                    }
                } else {
                    app.toast(ctx, format!("Assigned → {chosen}"));
                }
                *close = true;
            }
            Err(e) => app.toast_error(ctx, e),
        }
    }
}

pub fn show_quick_tags_dialog(
    app: &mut LightcraftApp,
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    text: &mut String,
    cursor: &mut usize,
    fresh: &mut bool,
    close: &mut bool,
) {
    let t = Tokens::get(ctx);
    let selection = app.session.selected();
    let sel_count = selection.len();
    ui.label(
        egui::RichText::new(format!(
            "Add or remove tags for {sel_count} selected photo{} (separate multiple tags with commas; Enter adds, Esc closes)",
            if sel_count == 1 { "" } else { "s" }
        ))
        .size(11.5)
        .color(t.text_dim),
    );
    ui.add_space(6.0);

    // Current keywords on the selected photos
    let chips = app.caches.keyword_chips(&app.session.catalog, &selection).to_vec();
    if !chips.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 4.0);
            for c in &chips {
                let leaf = c.path.rsplit('|').next().unwrap_or(&c.path);
                let label = if c.on_all() { format!("{leaf} ×") } else { format!("{leaf}* ×") };
                if ui
                    .button(egui::RichText::new(label).size(11.5))
                    .on_hover_text(format!("Click to remove tag “{}” from selection", c.path))
                    .clicked()
                {
                    let _ = app.run("photo.setMeta", json!({"removeKeywords": [c.path]}));
                }
            }
        });
        ui.add_space(6.0);
    }

    // Gather completions for the last comma-separated token
    let last_tok = text.rsplit(',').next().unwrap_or("").trim().to_lowercase();
    let have: Vec<String> = chips.iter().filter(|c| c.on_all()).map(|c| c.path.clone()).collect();
    let mut suggestions: Vec<String> = Vec::new();
    for r in &app.session.recent_keywords {
        if !have.iter().any(|h| h.eq_ignore_ascii_case(r))
            && (last_tok.is_empty() || r.to_lowercase().contains(&last_tok))
            && !suggestions.iter().any(|s| s.eq_ignore_ascii_case(r))
        {
            suggestions.push(r.clone());
        }
    }
    let comp = app.caches.completions(&app.session.catalog, &have, &last_tok);
    for kw in &comp.keywords {
        if !suggestions.iter().any(|s| s.eq_ignore_ascii_case(kw)) {
            suggestions.push(kw.clone());
        }
    }

    if *cursor >= suggestions.len() {
        *cursor = 0;
    }

    let mut trigger_add_text = false;
    let mut trigger_pick_suggestion = false;

    ui.input_mut(|i| {
        if i.consume_key(Modifiers::NONE, Key::ArrowDown) && !suggestions.is_empty() {
            *cursor = (*cursor + 1).min(suggestions.len().saturating_sub(1));
        } else if i.consume_key(Modifiers::NONE, Key::ArrowUp) && !suggestions.is_empty() {
            *cursor = cursor.saturating_sub(1);
        } else if i.consume_key(Modifiers::NONE, Key::Tab) && !suggestions.is_empty() {
            trigger_pick_suggestion = true;
        } else if i.consume_key(Modifiers::NONE, Key::Enter) {
            trigger_add_text = true;
        }
    });

    let prev = text.clone();
    let resp = ui
        .add(egui::TextEdit::singleline(text).hint_text("Type tag(s), e.g. Portrait, Vacation (Tab completes, Enter applies)…").desired_width(440.0));
    register(ui, "field:quickTagsInput", resp.rect);
    if *fresh {
        resp.request_focus();
        *fresh = false;
    }
    if *text != prev {
        *cursor = 0;
    }

    if trigger_pick_suggestion && let Some(chosen) = suggestions.get(*cursor).cloned() {
        let mut parts: Vec<String> = text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if !parts.is_empty() {
            parts.pop();
        }
        parts.push(chosen);
        *text = format!("{}, ", parts.join(", "));
        resp.request_focus();
    }

    if !suggestions.is_empty() {
        ui.add_space(4.0);
        egui::ScrollArea::vertical().id_salt("quick-tags-suggestions").max_height(160.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.set_min_width(420.0);
            for (idx, kw) in suggestions.iter().take(40).enumerate() {
                let is_cur = idx == *cursor;
                let r = ui.selectable_label(is_cur, format!("🏷 {kw}"));
                register(ui, format!("quickTagOption:{kw}"), r.rect);
                if r.clicked() {
                    let _ = app.run("photo.setMeta", json!({"addKeywords": [kw]}));
                    resp.request_focus();
                }
            }
        });
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Done (Esc)").clicked() {
            *close = true;
        }
        let add_btn = ui.button(egui::RichText::new("Add Tag(s) (Enter)").color(t.accent));
        register(ui, "button:quickTagsAdd", add_btn.rect);
        if add_btn.clicked() {
            trigger_add_text = true;
        }
    });

    if trigger_add_text {
        let kws: Vec<String> = text.split(',').map(|s| app.session.catalog.typed_keyword(s)).filter(|s| !s.is_empty()).collect();
        if kws.is_empty() {
            if let Some(chosen) = suggestions.get(*cursor).cloned() {
                let _ = app.run("photo.setMeta", json!({"addKeywords": [chosen]}));
            } else {
                *close = true;
            }
        } else {
            match app.run("photo.setMeta", json!({"addKeywords": kws})) {
                Ok(_) => {
                    app.toast(ctx, format!("Added {} tag(s)", kws.len()));
                    text.clear();
                    resp.request_focus();
                }
                Err(e) => app.toast_error(ctx, e),
            }
        }
    }
}

pub fn show_crop_preview_dialog(
    app: &mut LightcraftApp,
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    aspect: &mut String,
    index: &mut usize,
    close: &mut bool,
) {
    let t = Tokens::get(ctx);
    let ids = app.session.selected();
    if ids.is_empty() {
        ui.label("No photos selected.");
        if ui.button("Close").clicked() {
            *close = true;
        }
        return;
    }
    if *index >= ids.len() {
        *index = 0;
    }
    let cur_id = ids.get(*index).copied().unwrap_or(PhotoId(0));
    let Some(photo) = app.session.catalog.photo(cur_id).cloned() else { return };

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Aspect Ratio:").color(t.text_dim));
        for (lbl, key) in [
            ("16 : 9", "16x9"),
            ("24 : 10", "24x10"),
            ("16 : 10", "16x10"),
            ("4 : 3", "4x3"),
            ("3 : 2", "2x3"),
            ("1 : 1", "1x1"),
            ("Original", "original"),
        ] {
            let sel = aspect == key;
            let b = ui.selectable_label(sel, lbl);
            register(ui, format!("cropPreviewAspect:{key}"), b.rect);
            if b.clicked() {
                *aspect = key.to_string();
            }
        }
    });

    ui.add_space(6.0);

    // Compute preview crop rect in normalized coordinates
    let swap = photo.develop.orientation.swaps_axes();
    let (w, h) = if swap { (photo.height.max(1) as f64, photo.width.max(1) as f64) } else { (photo.width.max(1) as f64, photo.height.max(1) as f64) };
    let ratio = if aspect == "original" {
        Some(w / h)
    } else {
        aspect.split_once(['x', ':']).and_then(|(x, y)| {
            let x = x.parse::<f64>().ok()?;
            let y = y.parse::<f64>().ok()?;
            (x > 0.0 && y > 0.0).then(|| if (w >= h) == (x >= y) { x / y } else { y / x })
        })
    };
    let geom = lightcraft_geom::crop_fit_angle(w, h, photo.develop.crop.geometry.angle, ratio);
    let r_norm = geom.rect;
    let out_w = (w * r_norm.width()).round() as u32;
    let out_h = (h * r_norm.height()).round() as u32;

    ui.horizontal(|ui| {
        if ids.len() > 1 {
            if ui.add_enabled(*index > 0, egui::Button::new("◀ Prev")).clicked() {
                *index = index.saturating_sub(1);
            }
            ui.label(format!("Photo {} of {}", *index + 1, ids.len()));
            if ui.add_enabled(*index + 1 < ids.len(), egui::Button::new("Next ▶")).clicked() {
                *index = (*index + 1).min(ids.len().saturating_sub(1));
            }
            ui.separator();
        }
        ui.label(egui::RichText::new(format!("{}  ({} × {}  →  {} × {} px)", photo.file_name, w as u32, h as u32, out_w, out_h)).color(t.text_label));
    });

    ui.add_space(6.0);

    // Render thumbnail + crop frame overlay
    super::grid::request_thumb(app, cur_id, 512, 20);
    let preview_box = vec2(520.0, 320.0);
    let (canvas, _) = ui.allocate_exact_size(preview_box, Sense::hover());
    let p = ui.painter_at(canvas);
    p.rect_filled(canvas, 4.0, Color32::from_gray(22));

    let img_aspect = (w / h).max(0.05) as f32;
    let fit_size = if canvas.width() / canvas.height() > img_aspect {
        vec2((canvas.height() - 16.0) * img_aspect, canvas.height() - 16.0)
    } else {
        vec2(canvas.width() - 16.0, (canvas.width() - 16.0) / img_aspect)
    };
    let img_rect = Rect::from_center_size(canvas.center(), fit_size);

    if let Some(tex) = app.renderer.thumb(cur_id) {
        p.image(tex.tex.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    } else {
        p.rect_filled(img_rect, 0.0, Color32::from_gray(42));
        p.text(img_rect.center(), Align2::CENTER_CENTER, "Loading preview…", t.font(12.5), t.text_dim);
    }

    // Draw darkened mask outside the crop rectangle and bright border around the kept region
    let crop_rect = Rect::from_min_max(
        pos2(img_rect.left() + img_rect.width() * r_norm.x0 as f32, img_rect.top() + img_rect.height() * r_norm.y0 as f32),
        pos2(img_rect.left() + img_rect.width() * r_norm.x1 as f32, img_rect.top() + img_rect.height() * r_norm.y1 as f32),
    );
    let shade = Color32::from_black_alpha(155);
    p.rect_filled(Rect::from_min_max(img_rect.min, pos2(img_rect.right(), crop_rect.top())), 0.0, shade);
    p.rect_filled(Rect::from_min_max(pos2(img_rect.left(), crop_rect.bottom()), img_rect.max), 0.0, shade);
    p.rect_filled(Rect::from_min_max(pos2(img_rect.left(), crop_rect.top()), pos2(crop_rect.left(), crop_rect.bottom())), 0.0, shade);
    p.rect_filled(Rect::from_min_max(pos2(crop_rect.right(), crop_rect.top()), pos2(img_rect.right(), crop_rect.bottom())), 0.0, shade);
    p.rect_stroke(crop_rect, 0.0, Stroke::new(2.0, t.accent), StrokeKind::Inside);

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        let apply_label = format!("Apply {} Crop to {} Selected Photo{}", aspect.replace('x', ":"), ids.len(), if ids.len() == 1 { "" } else { "s" });
        let apply_btn = ui.button(egui::RichText::new(apply_label).color(t.accent));
        register(ui, "button:cropPreviewApply", apply_btn.rect);
        if apply_btn.clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
            match app.run("crop.aspect", json!({"aspect": aspect.clone(), "selection": true})) {
                Ok(_) => {
                    app.toast(ctx, format!("Applied {} crop to {} photo(s)", aspect.replace('x', ":"), ids.len()));
                    *close = true;
                }
                Err(e) => app.toast_error(ctx, e),
            }
        }
    });
}

pub fn show_convert_screenshots_dialog(
    app: &mut LightcraftApp,
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    from_pattern: &mut String,
    to_pattern: &mut String,
    hide_unrenamed: &mut bool,
    all_visible: &mut bool,
    close: &mut bool,
) {
    use std::hash::{Hash, Hasher};

    let t = Tokens::get(ctx);

    ui.horizontal(|ui| {
        ui.label("From pattern:");
        let from_lbl = ScreenshotPattern::from_id(from_pattern).map(|p| p.label()).unwrap_or(from_pattern.as_str());
        let from_drop = crate::widgets::dropdown(ui, "screenshotFromPattern", from_lbl, t.font(12.0), t.text);
        egui::Popup::menu(&from_drop).show(|ui| {
            for pat in ScreenshotPattern::CONVERTIBLE {
                if ui.selectable_label(from_pattern == pat.id(), pat.label()).clicked() {
                    *from_pattern = pat.id().to_string();
                }
            }
        });

        ui.label("→  To pattern:");
        let to_lbl = ScreenshotPattern::from_id(to_pattern).map(|p| p.label()).unwrap_or(to_pattern.as_str());
        let to_drop = crate::widgets::dropdown(ui, "screenshotToPattern", to_lbl, t.font(12.0), t.text);
        egui::Popup::menu(&to_drop).show(|ui| {
            for pat in ScreenshotPattern::CONVERTIBLE {
                if ui.selectable_label(to_pattern == pat.id(), pat.label()).clicked() {
                    *to_pattern = pat.id().to_string();
                }
            }
        });
    });

    ui.horizontal(|ui| {
        ui.checkbox(hide_unrenamed, "Hide files without rename result");
        ui.checkbox(all_visible, "All photos in current view");
    });

    // Cache `detectScreenshots` and `convertScreenshots` preview results by input state so disk collision checks don't run every frame
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (from_pattern.as_str(), to_pattern.as_str(), *all_visible, app.session.catalog.revision, &app.session.selection.ids, app.session.active())
        .hash(&mut h);
    let preview_key = h.finish();
    let preview_cache_id = egui::Id::unique("screenshot-convert-preview-cache");
    let (det, preview_res) = match ctx.data(|d| d.get_temp::<(u64, Value, Value)>(preview_cache_id)) {
        Some((k, det, prev)) if k == preview_key => (det, prev),
        _ => {
            let det = app.session.execute("organizer.detectScreenshots", &json!({ "allVisible": *all_visible })).unwrap_or_else(|_| json!({}));
            let prev = app
                .session
                .execute(
                    "organizer.convertScreenshots",
                    &json!({
                        "fromPattern": from_pattern.clone(),
                        "toPattern": to_pattern.clone(),
                        "preview": true,
                        "allVisible": *all_visible,
                    }),
                )
                .unwrap_or_else(|_| json!({}));
            ctx.data_mut(|d| d.insert_temp(preview_cache_id, (preview_key, det.clone(), prev.clone())));
            (det, prev)
        }
    };

    // Pattern summary & mixed-pattern warning
    if let Some(counts) = det.get("counts").and_then(Value::as_array)
        && counts.len() > 1
    {
        let summary: Vec<String> =
            counts.iter().filter_map(|c| Some(format!("{}: {}", c.get("pattern")?.as_str()?, c.get("count")?.as_u64()?))).collect();
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("⚠ Warning: Files do not all share the same filename pattern ({})", summary.join(", ")))
                .size(11.5)
                .color(t.caution),
        );
    }

    let rows = preview_res.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    let count = preview_res.get("count").and_then(Value::as_u64).unwrap_or(0);

    ui.add_space(6.0);
    egui::ScrollArea::vertical().id_salt("screenshot-preview-table").max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.set_min_width(540.0);
        egui::Grid::new("screenshot-conversion-grid").striped(true).num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.label(egui::RichText::new("Original name").font(t.semibold(12.0)).color(t.text_dim));
            ui.label("");
            ui.label(egui::RichText::new("New name").font(t.semibold(12.0)).color(t.text_dim));
            ui.end_row();

            for r in &rows {
                let changed = r.get("changed").and_then(Value::as_bool).unwrap_or(false);
                if *hide_unrenamed && !changed {
                    continue;
                }
                let from = r.get("from").and_then(Value::as_str).unwrap_or("");
                let to = r.get("to").and_then(Value::as_str);
                ui.label(from);
                ui.label("→");
                match to {
                    Some(new_name) => {
                        ui.label(egui::RichText::new(new_name).color(if changed { t.text } else { t.text_dim }));
                    }
                    None => {
                        let det_pat = r.get("detectedPattern").and_then(Value::as_str).unwrap_or("UNKNOWN");
                        ui.label(egui::RichText::new(format!("-- no match ({det_pat}) --")).color(t.text_dim));
                    }
                }
                ui.end_row();
            }
        });
    });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        let btn = ui.add_enabled(
            count > 0 && from_pattern != to_pattern,
            egui::Button::new(egui::RichText::new(format!("Convert {count} Name{}", if count == 1 { "" } else { "s" })).color(t.accent)),
        );
        register(ui, "button:convertScreenshotsConfirm", btn.rect);
        if btn.clicked() {
            match app.run(
                "organizer.convertScreenshots",
                json!({
                    "fromPattern": from_pattern.clone(),
                    "toPattern": to_pattern.clone(),
                    "preview": false,
                    "allVisible": *all_visible,
                }),
            ) {
                Ok(r) => {
                    let n = r.get("count").and_then(Value::as_u64).unwrap_or(0);
                    app.toast(ctx, format!("Converted {n} screenshot filename{}", if n == 1 { "" } else { "s" }));
                    *close = true;
                }
                Err(e) => app.toast_error(ctx, e),
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub fn show_mystuff_pigallery_dialog(
    app: &mut LightcraftApp,
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    host: &mut String,
    pigallery_url: &mut String,
    games: &mut Vec<MyStuffGameEntry>,
    selected_game: &mut usize,
    game_filter: &mut String,
    media: &mut Vec<MyStuffMediaEntry>,
    error: &mut Option<String>,
    loaded_host: &mut Option<String>,
    close: &mut bool,
) {
    let t = Tokens::get(ctx);

    ui.horizontal(|ui| {
        ui.label("PiGallery2 URL:");
        ui.add(egui::TextEdit::singleline(pigallery_url).desired_width(380.0));
        if ui.button("Copy").clicked() {
            ctx.copy_text(pigallery_url.clone());
            app.toast(ctx, "Copied PiGallery2 URL");
        }
    });

    ui.add_space(4.0);
    let mut want_reload = loaded_host.as_deref() != Some(host.as_str());
    ui.horizontal(|ui| {
        ui.label("Database Host:");
        for h in ["shirogane", "127.0.0.1"] {
            if ui.selectable_label(host == h, h).clicked() && host != h {
                *host = h.to_string();
                want_reload = true;
            }
        }
        if ui.button("↻ Reload Games").clicked() {
            want_reload = true;
        }
    });

    if want_reload {
        let mut tmp = Dialog::MyStuffPigallery {
            host: host.clone(),
            pigallery_url: pigallery_url.clone(),
            games: games.clone(),
            selected_game: *selected_game,
            game_filter: game_filter.clone(),
            media: media.clone(),
            error: error.clone(),
            loaded_host: loaded_host.clone(),
        };
        reload_mystuff_host(app, &mut tmp, None);
        if let Dialog::MyStuffPigallery { games: g2, selected_game: s2, media: m2, error: e2, loaded_host: l2, .. } = tmp {
            *games = g2;
            *selected_game = s2;
            *media = m2;
            *error = e2;
            *loaded_host = l2;
        }
    }

    if let Some(err) = error {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(format!("⚠ {err}")).color(t.caution));
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Filter / Search Game:");
        let prev_f = game_filter.clone();
        ui.add(egui::TextEdit::singleline(game_filter).hint_text("Type game name…").desired_width(220.0));
        if *game_filter != prev_f && !game_filter.trim().is_empty() {
            let fl = game_filter.trim().to_lowercase();
            if let Some(pos) = games.iter().position(|g| g.name.to_lowercase().contains(&fl)) {
                *selected_game = pos;
                *media = games.get(pos).map(|g| g.media.clone()).unwrap_or_default();
            }
        }
    });

    let cur_game_name = games.get(*selected_game).map(|g| g.name.as_str()).unwrap_or("— No game selected —");
    ui.horizontal(|ui| {
        ui.label("Select Game:");
        let drop = crate::widgets::dropdown(ui, "mystuffGameSelect", cur_game_name, t.font(12.5), t.text);
        egui::Popup::menu(&drop).show(|ui| {
            let fl = game_filter.trim().to_lowercase();
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for (i, g) in games.iter().enumerate() {
                    if !fl.is_empty() && !g.name.to_lowercase().contains(&fl) {
                        continue;
                    }
                    if ui.selectable_label(i == *selected_game, &g.name).clicked() {
                        *selected_game = i;
                        *media = g.media.clone();
                        ui.close();
                    }
                }
            });
        });
    });

    ui.add_space(6.0);
    ui.label(egui::RichText::new("Media Entries:").font(t.semibold(12.0)).color(t.text_dim));

    let mut delete_idx: Option<usize> = None;
    egui::ScrollArea::vertical().id_salt("mystuff-media-rows").max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.set_min_width(520.0);
        for (idx, row) in media.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.add(egui::TextEdit::singleline(&mut row.name).desired_width(120.0));
                ui.label("URL:");
                ui.add(egui::TextEdit::singleline(&mut row.location).desired_width(310.0));
                if ui.button("✕").on_hover_text("Remove entry").clicked() {
                    delete_idx = Some(idx);
                }
            });
        }
    });
    if let Some(idx) = delete_idx
        && idx < media.len()
    {
        media.remove(idx);
    }

    ui.add_space(4.0);
    if ui.button("+ Add New Entry (Screenshots)").clicked() {
        media.push(MyStuffMediaEntry { name: "Screenshots".into(), location: pigallery_url.clone() });
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        let can_save = games.get(*selected_game).is_some();
        let save_btn = ui.add_enabled(can_save, egui::Button::new(egui::RichText::new("Save to MyStuff").color(t.accent)));
        register(ui, "button:mystuffSave", save_btn.rect);
        if save_btn.clicked()
            && let Some(game) = games.get(*selected_game).cloned()
        {
            match app.run(
                "organizer.mystuffSaveMedia",
                json!({
                    "host": host.clone(),
                    "gameId": game.id,
                    "gameName": game.name,
                    "media": media.clone(),
                }),
            ) {
                Ok(_) => {
                    app.toast(ctx, format!("Updated media for “{}” in MyStuff ({host})", game.name));
                    *close = true;
                }
                Err(e) => *error = Some(e),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use crate::headless::HeadlessView;
    use crate::state::{Dialog, RightPanel, ViewMode};

    #[test]
    fn m_and_t_shortcuts_open_quick_move_and_quick_tags_in_grid_and_fullscreen() {
        let session = lightcraft_engine::Session::with_demo();
        let mut app = LightcraftApp::new(session, Services::default());
        app.ui.view = ViewMode::PhotoGrid;

        let mut view = HeadlessView::new();
        // Press M in PhotoGrid -> opens QuickMove dialog
        let raw_m = HeadlessView::raw_input(
            vec2(1200.0, 800.0),
            1.0,
            0.1,
            vec![egui::Event::Key {
                key: egui::Key::M,
                physical_key: Some(egui::Key::M),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        view.run(raw_m, |ui| {
            crate::shortcuts::handle(&mut app, ui.ctx());
        });
        assert!(matches!(app.ui.dialog, Some(Dialog::QuickMove { .. })));
        app.ui.dialog = None;

        // Press T in PhotoGrid -> opens QuickTags dialog
        let raw_t = HeadlessView::raw_input(
            vec2(1200.0, 800.0),
            1.0,
            0.2,
            vec![egui::Event::Key {
                key: egui::Key::T,
                physical_key: Some(egui::Key::T),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        view.run(raw_t, |ui| {
            crate::shortcuts::handle(&mut app, ui.ctx());
        });
        assert!(matches!(app.ui.dialog, Some(Dialog::QuickTags { .. })));
        app.ui.dialog = None;

        // In Detail view (not fullscreen), M still opens Masking panel
        app.ui.view = ViewMode::Detail;
        app.ui.fullscreen = false;
        let raw_m_detail = HeadlessView::raw_input(
            vec2(1200.0, 800.0),
            1.0,
            0.3,
            vec![egui::Event::Key {
                key: egui::Key::M,
                physical_key: Some(egui::Key::M),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        view.run(raw_m_detail, |ui| {
            crate::shortcuts::handle(&mut app, ui.ctx());
        });
        assert_eq!(app.ui.right, RightPanel::Masking);
        assert!(app.ui.dialog.is_none());

        // Press F to enter Fullscreen, then press M -> opens QuickMove dialog!
        app.run("view.fullScreenPreview", json!({})).unwrap();
        assert!(app.ui.fullscreen);
        let raw_m_fs = HeadlessView::raw_input(
            vec2(1200.0, 800.0),
            1.0,
            0.4,
            vec![egui::Event::Key {
                key: egui::Key::M,
                physical_key: Some(egui::Key::M),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        view.run(raw_m_fs, |ui| {
            crate::shortcuts::handle(&mut app, ui.ctx());
        });
        assert!(matches!(app.ui.dialog, Some(Dialog::QuickMove { .. })));
    }

    #[test]
    fn organizer_bar_and_dialogs_render_and_stage_subfolders() {
        let mut session = lightcraft_engine::Session::with_demo();
        let ids: Vec<u64> = session.visible_cloned().iter().take(2).map(|p| p.0).collect();
        session.execute("library.select", &json!({"ids": ids})).unwrap();

        let mut app = LightcraftApp::new(session, Services::default());
        app.ui.view = ViewMode::PhotoGrid;
        app.run("view.organizerBar", json!({"on": true})).unwrap();
        assert!(app.ui.organizer_bar);

        // Stage subfolder "_Lea/alleine" for the 2 selected photos
        app.run("organizer.stageSubfolder", json!({"subfolder": "_Lea/alleine"})).unwrap();
        assert_eq!(app.session.organizer.staged_moves.len(), 2);

        // Batch crop both selected photos to 24:10
        app.run("crop.aspect", json!({"aspect": "24x10", "orientation": "given", "selection": true})).unwrap();
        for id in &ids {
            let d = app.session.develop_of(PhotoId(*id)).unwrap();
            assert_eq!(d.crop.aspect, Some((2400, 1000)));
        }

        // Render each Organizer dialog headlessly to verify zero panics and widget registration
        let mut view = HeadlessView::new();
        for cmd in ["dialog.quickMove", "dialog.quickTags", "dialog.cropPreview", "dialog.convertScreenshots", "dialog.mystuffPigallery"] {
            if cmd == "dialog.mystuffPigallery" {
                app.session
                    .organizer
                    .mock_games
                    .insert("shirogane".into(), vec![MyStuffGameEntry { id: "1".into(), name: "Cyberpunk 2077".into(), media: vec![] }]);
            }
            app.run(cmd, json!({})).unwrap();
            assert!(app.ui.dialog.is_some(), "{cmd} should open a dialog");
            let raw = HeadlessView::raw_input(vec2(1280.0, 800.0), 1.0, 0.5, vec![]);
            view.run(raw, |ui| {
                super::bar(&mut app, ui);
                crate::panels::dialogs::show(&mut app, ui.ctx(), false);
            });
            app.ui.dialog = None;
        }
    }

    #[test]
    fn empty_folder_shows_delete_folder_button_in_bar() {
        let tmp = std::env::temp_dir().join(format!("lc-ui-del-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let empty_dir = tmp.join("empty_todo");
        std::fs::create_dir_all(&empty_dir).unwrap();

        let mut session = lightcraft_engine::Session::new().with_fs();
        session.execute("library.browse", &json!({"path": empty_dir.to_string_lossy()})).unwrap();

        let mut app = LightcraftApp::new(session, Services::default());
        app.ui.view = ViewMode::PhotoGrid;
        assert!(super::should_show_bar(&app));

        let mut view = HeadlessView::new();
        let raw = HeadlessView::raw_input(vec2(1280.0, 800.0), 1.0, 0.5, vec![]);
        view.run(raw, |ui| {
            super::bar(&mut app, ui);
        });

        let widgets = crate::widgets::take_registry(&view.ctx);
        let has_del_btn = widgets.iter().any(|(w, _)| w == "button:organizerDeleteFolder");
        assert!(has_del_btn, "Delete Folder button should be registered in the widget map");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
