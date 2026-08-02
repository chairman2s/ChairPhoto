pub mod bundle;
pub mod burst;
pub mod catalog;
#[cfg(feature = "collage")]
pub mod collage;
mod commands;
mod external_edit;
pub mod export;
#[cfg(feature = "flickr")]
pub mod flickr;
pub mod image_pool;
#[cfg(feature = "instagram")]
pub mod instagram;
#[cfg(feature = "localsend")]
pub mod localsend;
pub mod metadata;
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub mod oauth1;
pub mod phash;
pub mod phash_indexer;
pub mod plugins;
mod protocol;
#[cfg(feature = "raw")]
pub mod raw;
#[cfg(feature = "smugmug")]
pub mod smugmug;
pub mod rapidraw;
pub mod scanner;
pub mod sharpness;
pub mod sharpness_indexer;
pub mod sharpness_regions;
#[cfg(feature = "slideshow")]
pub mod slideshow;
pub mod thumbnails;
mod volume_health;
pub mod xmp;

use commands::AppState;
use image_pool::ImagePool;
use protocol::{handle_image_request, ImageKind};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebKitGTK's DMABUF renderer crashes on NVIDIA + Wayland with
    // "Error 71 (Protocol error) dispatching to Wayland display". Disabling it
    // before the webview is created fixes startup. Linux-only; harmless elsewhere.
    // Must run before any webview initialization.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    let builder = tauri::Builder::default();

    // chairphoto://<uuid> deep links. single-instance must be the FIRST plugin: its
    // deep-link feature forwards a second launch's URL argv to the running instance
    // (Linux/Windows), which then re-fires the deep-link event.
    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_deep_link::init());

    builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        // Serve images natively (no base64/IPC) — see src/protocol.rs.
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, req, responder| {
            handle_image_request(ImageKind::Thumb, ctx, req, responder);
        })
        .register_asynchronous_uri_scheme_protocol("preview", |ctx, req, responder| {
            handle_image_request(ImageKind::Preview, ctx, req, responder);
        })
        .register_asynchronous_uri_scheme_protocol("zoom", |ctx, req, responder| {
            handle_image_request(ImageKind::Zoom, ctx, req, responder);
        })
        // <video> on WebKitGTK uses GStreamer, which can't read custom URI schemes — so we
        // serve videos over a loopback HTTP server (range-capable) it can fetch instead.
        .setup(|app| {
            // Dev builds aren't installed, so no .desktop/registry entry registers the
            // chairphoto:// scheme — register it at runtime (writes a handler .desktop
            // pointing at this binary on Linux). Bundles register via tauri.conf.json.
            #[cfg(all(debug_assertions, any(target_os = "linux", windows)))]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                if let Err(e) = app.deep_link().register_all() {
                    eprintln!("deep-link dev registration failed: {e}");
                }

                // xdg-open (what Electron apps like Obsidian launch links with) takes the
                // FIRST WORD of the .desktop Exec line verbatim and `command -v`s it — the
                // quoted path register_all() writes ("/…/chairphoto") therefore "doesn't
                // exist" and xdg-open silently falls back to the default browser. gio
                // parses the quoting fine; only xdg-open breaks. Strip the quotes (our dev
                // path has no spaces). register_all() re-quotes on every startup, so this
                // must follow it every time. Bundles are unaffected (Exec=chairphoto %u).
                #[cfg(target_os = "linux")]
                if let (Ok(exe), Ok(dir)) =
                    (tauri::utils::platform::current_exe(), app.path().data_dir())
                {
                    let exec = exe.to_string_lossy().to_string();
                    if !exec.contains(' ') {
                        let f = dir.join(format!(
                            "applications/{}-handler.desktop",
                            exe.file_name().unwrap_or_default().to_string_lossy()
                        ));
                        if let Ok(s) = std::fs::read_to_string(&f) {
                            let quoted = format!("Exec=\"{exec}\" %u");
                            if s.contains(&quoted) {
                                let fixed = s.replace(&quoted, &format!("Exec={exec} %u"));
                                if let Err(e) = std::fs::write(&f, fixed) {
                                    eprintln!("couldn't unquote deep-link handler Exec: {e}");
                                }
                            }
                        }
                    }
                }
            }

            match protocol::start_video_server(app.handle().clone()) {
                Ok(port) => eprintln!("video server on http://127.0.0.1:{port}"),
                Err(e) => eprintln!("failed to start video server: {e}"),
            }

            // Build the bounded LIFO image pool and make it available to the URI scheme
            // handlers via Tauri's state system.
            let n_threads = image_pool::default_thread_count();
            eprintln!("image pool: {n_threads} worker threads");
            let handle = app.handle().clone();
            let runner: image_pool::Runner = std::sync::Arc::new(move |key| {
                protocol::render_bytes(&handle, key)
            });
            // `manage` holds the Arc for the app's lifetime — that alone keeps the pool
            // (and its worker threads) alive.
            app.manage(ImagePool::start_with_runner(n_threads, runner));

            // H16b: Register the sharpness analyzer so newly decoded previews are scored
            // on the fly — scoring rides the decode that was already paid for (I7b hook)
            // instead of re-reading the file. Score-on-index handles the backfill.
            //
            // The closure captures a clone of the catalog Arc. When a decode fires (inside
            // the thumbnail pool's worker threads), the closure:
            //   1. Computes the sharpness score from the already-decoded image (pure math).
            //   2. Briefly locks the catalog to resolve the absolute path → photo_id.
            //   3. Writes the score if the photo is not yet scored (sharpness IS NULL guard).
            //
            // The lock scope is narrow (one SELECT + one UPDATE) so it does not meaningfully
            // contend with UI reads. The run_analyzers fix (snapshot before invoke) ensures
            // the registry lock is NOT held during this work.
            {
                let catalog_arc = app.state::<AppState>().catalog.clone();
                thumbnails::register_analyzer(std::sync::Arc::new(move |img, path| {
                    use crate::sharpness_indexer::{
                        photo_af_point, score_image_regions, write_sharpness, RegionInputs,
                    };
                    use rusqlite::OptionalExtension as _;

                    let guard = match catalog_arc.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    let catalog = match guard.as_ref() {
                        Some(c) => c,
                        None => return,
                    };
                    // Convert the absolute path to a catalog-root-relative path — the same
                    // key used in photos.path. If the file is outside the catalog root
                    // (e.g. a card import source path), skip silently.
                    let root = catalog.root();
                    let rel = match path.strip_prefix(root) {
                        Ok(r) => r.to_string_lossy().to_string(),
                        Err(_) => return,
                    };
                    // Look up the photo only if it is not yet scored; don't overwrite an
                    // existing score (e.g. from the batch indexer) and don't write 0 for
                    // photos that have no catalog row yet (not imported).
                    let photo_id: Option<i64> = catalog
                        .conn()
                        .query_row(
                            "SELECT id FROM photos WHERE path = ?1 AND sharpness IS NULL",
                            rusqlite::params![rel],
                            |r| r.get(0),
                        )
                        .optional()
                        .ok()
                        .flatten();
                    if let Some(id) = photo_id {
                        // Region-aware scoring (H16c): faces → AF point → tiles. Regions are
                        // read under the same lock we already hold. Faces are only available
                        // with the `faces` feature; without it the chain falls through to
                        // AF/tile. Scoring rides the decode the pool already paid for.
                        let af_point = photo_af_point(catalog.conn(), id);
                        #[cfg(feature = "faces")]
                        let face_boxes =
                            crate::plugins::faces::store::face_boxes_for_photo(catalog.conn(), id)
                                .unwrap_or_default();
                        #[cfg(not(feature = "faces"))]
                        let face_boxes = Vec::new();
                        let regions = RegionInputs { face_boxes, af_point };
                        let (score, method) = score_image_regions(img, &regions);
                        let _ = write_sharpness(catalog.conn(), id, score, method);
                    }
                }));
            }

            // H15a: Register the perceptual-hash analyzer so newly decoded previews are
            // hashed on the fly (I7b hook — one decode, many analyzers). Unlike sharpness,
            // the dHash is resolution-invariant, so any decode that reaches the analyzers
            // (gated at PREVIEW_MAX) is fine. `hash_and_store` writes only when the photo
            // is not yet hashed (phash IS NULL), so it never fights the batch indexer and
            // never inserts for a file with no catalog row. The batch `index_phashes`
            // command backfills photos imported before this hook existed.
            {
                let catalog_arc = app.state::<AppState>().catalog.clone();
                thumbnails::register_analyzer(std::sync::Arc::new(move |img, path| {
                    use rusqlite::OptionalExtension as _;

                    let guard = match catalog_arc.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    let catalog = match guard.as_ref() {
                        Some(c) => c,
                        None => return,
                    };
                    let root = catalog.root();
                    let rel = match path.strip_prefix(root) {
                        Ok(r) => r.to_string_lossy().to_string(),
                        Err(_) => return,
                    };
                    // Only look up unhashed photos so we don't decode-and-store redundantly.
                    let photo_id: Option<i64> = catalog
                        .conn()
                        .query_row(
                            "SELECT id FROM photos WHERE path = ?1 AND phash IS NULL",
                            rusqlite::params![rel],
                            |r| r.get(0),
                        )
                        .optional()
                        .ok()
                        .flatten();
                    if let Some(id) = photo_id {
                        crate::phash_indexer::hash_and_store(catalog.conn(), id, img);
                    }
                }));
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::switch_catalog,
            commands::list_recent_catalogs,
            commands::init_catalog,
            commands::get_library_root,
            commands::set_library_root,
            commands::rescan_library,
            commands::drain_enrichment_queue,
            commands::plugin_features,
            commands::get_setting,
            commands::set_setting,
            commands::list_external_modules,
            commands::get_modules_dir,
            commands::ai_suggest_tags,
            commands::ai_ollama_models,
            commands::ai_default_prompt,
            commands::ai_get_suggestions,
            commands::ai_accept_suggestion,
            commands::ai_reject_suggestion,
            commands::ai_grouped_estimate,
            commands::ai_suggest_tags_grouped,
            commands::list_photos,
            commands::list_facets,
            commands::distinct_photo_values,
            commands::list_import_batches,
            commands::export_photos,
            commands::export_bundle,
            commands::import_bundle_cmd,
            commands::preview_bundle,
            #[cfg(feature = "instagram")]
            commands::post_to_instagram,
            #[cfg(feature = "instagram")]
            commands::build_instagram_caption,
            #[cfg(feature = "flickr")]
            commands::flickr_begin_auth,
            #[cfg(feature = "flickr")]
            commands::flickr_complete_auth,
            #[cfg(feature = "flickr")]
            commands::flickr_connected,
            #[cfg(feature = "flickr")]
            commands::post_to_flickr,
            #[cfg(feature = "flickr")]
            commands::flickr_suggest_tags,
            #[cfg(feature = "flickr")]
            commands::flickr_import_published,
            #[cfg(feature = "flickr")]
            commands::flickr_import_apply,
            #[cfg(feature = "smugmug")]
            commands::smugmug_begin_auth,
            #[cfg(feature = "smugmug")]
            commands::smugmug_complete_auth,
            #[cfg(feature = "smugmug")]
            commands::smugmug_connected,
            #[cfg(feature = "smugmug")]
            commands::smugmug_list_albums,
            #[cfg(feature = "smugmug")]
            commands::smugmug_create_album,
            #[cfg(feature = "smugmug")]
            commands::post_to_smugmug,
            #[cfg(feature = "localsend")]
            commands::localsend_discover,
            #[cfg(feature = "localsend")]
            commands::localsend_send,
            commands::backup_photo,
            commands::offload_photo,
            commands::restore_photo,
            commands::remove_photo_from_catalog,
            commands::relocate_photo,
            commands::find_unavailable_photos,
            commands::purge_unavailable_photos,
            commands::find_empty_photos,
            commands::purge_empty_photos,
            commands::vacuum_catalog,
            commands::video_server_port,
            commands::apply_offload_policy,
            commands::scan_nas_folder_cmd,
            commands::list_pending_operations,
            commands::enqueue_operation,
            commands::reconcile_now,
            commands::assemble_hashtag_bundle,
            commands::get_photo,
            commands::get_photo_by_uuid,
            commands::photo_path,
            commands::set_rating,
            commands::set_label,
            commands::set_pick_state,
            commands::list_tags,
            commands::create_tag,
            commands::assign_tag,
            commands::remove_tag,
            commands::get_photo_tags,
            commands::get_photo_metadata,
            commands::get_iptc,
            commands::set_iptc,
            commands::rename_tag,
            commands::delete_tag,
            commands::move_tag,
            commands::suggest_tags_by_time,
            commands::apply_auto_tags,
            commands::list_tag_groups,
            commands::create_tag_group,
            commands::rename_tag_group,
            commands::delete_tag_group,
            commands::get_group_members,
            commands::recently_used_tags,
            commands::add_tag_to_group,
            commands::remove_tag_from_group,
            commands::get_edit_record,
            commands::set_edit_record,
            commands::render_edit,
            commands::render_edit_batch,
            commands::list_luts,
            commands::import_lut,
            commands::delete_lut,
            commands::list_versions,
            commands::create_version,
            commands::rename_version,
            commands::set_version_edit,
            commands::delete_version,
            commands::duplicate_version,
            commands::reorder_versions,
            commands::version_counts,
            commands::list_publications,
            commands::record_publication,
            commands::delete_publication,
            commands::list_albums,
            commands::create_album,
            commands::rename_album,
            commands::delete_album,
            commands::add_photos_to_album,
            commands::remove_photos_from_album,
            commands::list_smart_albums,
            commands::create_smart_album,
            commands::rename_smart_album,
            commands::set_smart_album_rule,
            commands::delete_smart_album,
            commands::reorder_smart_albums,
            commands::smart_album_count,
            commands::set_tag_description,
            commands::get_tag_exportable,
            commands::set_tag_exportable,
            commands::get_tag_private,
            commands::set_tag_private,
            commands::tidy_redundant_tags,
            commands::photo_tag_graph,
            commands::library_graph,
            commands::catalog_stats,
            commands::list_tag_terms,
            commands::add_tag_term,
            commands::update_tag_term,
            commands::set_term_export,
            commands::remove_tag_term,
            commands::list_languages,
            commands::tag_export_preview,
            commands::list_volumes,
            commands::add_volume,
            commands::remove_volume,
            commands::photo_statuses,
            commands::get_photo_locations,
            commands::scan_folder_cmd,
            commands::ingest_from_card_cmd,
            commands::list_card_photos_cmd,
            commands::card_thumbnail,
            commands::cache_images,
            commands::get_thumbnail,
            commands::get_preview,
            commands::rotate_photo,
            commands::list_stack_children,
            commands::stack_photo,
            commands::unstack_photo,
            commands::pair_raw_jpeg_stacks,
            external_edit::available_editors,
            external_edit::develop_in_editor,
            external_edit::import_developed,
            rapidraw::rapidraw_available,
            rapidraw::edit_in_rapidraw,
            rapidraw::cancel_rapidraw,
            #[cfg(feature = "collage")]
            commands::make_collage,
            #[cfg(feature = "collage")]
            commands::collage_preview,
            #[cfg(feature = "collage")]
            commands::collage_auto_arrange,
            #[cfg(feature = "collage")]
            commands::make_collage_freeform,
            #[cfg(feature = "collage")]
            commands::save_collage_to_catalog,
            #[cfg(feature = "slideshow")]
            commands::make_slideshow,
            #[cfg(feature = "map")]
            commands::list_fences,
            #[cfg(feature = "map")]
            commands::create_fence,
            #[cfg(feature = "map")]
            commands::update_fence,
            #[cfg(feature = "map")]
            commands::delete_fence,
            #[cfg(feature = "map")]
            commands::apply_fence,
            #[cfg(feature = "map")]
            commands::apply_all_fences,
            #[cfg(feature = "map")]
            commands::map_photo_points,
            #[cfg(feature = "map")]
            commands::set_photo_gps,
            #[cfg(feature = "map")]
            commands::reverse_geocode_photo,
            #[cfg(feature = "map")]
            commands::geocode_to_iptc,
            #[cfg(feature = "map")]
            commands::geocode_all_to_iptc,
            #[cfg(feature = "faces")]
            commands::faces_models_status,
            #[cfg(feature = "faces")]
            commands::faces_download_models,
            #[cfg(feature = "faces")]
            commands::faces_inference_info,
            #[cfg(feature = "faces")]
            commands::faces_set_indexing_speed,
            #[cfg(feature = "faces")]
            commands::faces_index_photos,
            #[cfg(feature = "faces")]
            commands::faces_index_cancel,
            #[cfg(feature = "faces")]
            commands::faces_index_status,
            #[cfg(feature = "faces")]
            commands::faces_run_matching,
            #[cfg(feature = "faces")]
            commands::faces_accept,
            #[cfg(feature = "faces")]
            commands::faces_reject,
            #[cfg(feature = "faces")]
            commands::faces_ignore,
            #[cfg(feature = "faces")]
            commands::faces_assign,
            #[cfg(feature = "faces")]
            commands::faces_name_cluster,
            #[cfg(feature = "faces")]
            commands::faces_add_manual,
            #[cfg(feature = "faces")]
            commands::faces_delete_drawn,
            #[cfg(feature = "faces")]
            commands::faces_for_photo,
            #[cfg(feature = "faces")]
            commands::faces_people_summary,
            #[cfg(feature = "faces")]
            commands::faces_cluster_summary,
            #[cfg(feature = "faces")]
            commands::faces_suggestion_list,
            commands::index_sharpness,
            commands::sharpness_index_cancel,
            commands::index_phashes,
            commands::phash_index_cancel,
            commands::analyze_burst_sharpness,
            #[cfg(feature = "smarttags")]
            commands::smarttags_model_status,
            #[cfg(feature = "smarttags")]
            commands::smarttags_download_model,
            #[cfg(feature = "smarttags")]
            commands::smarttags_index_photos,
            #[cfg(feature = "smarttags")]
            commands::smarttags_index_cancel,
            #[cfg(feature = "smarttags")]
            commands::smarttags_index_status,
            #[cfg(feature = "smarttags")]
            commands::smarttags_suggest_tags,
            #[cfg(feature = "smarttags")]
            commands::smarttags_load_suggestions,
            #[cfg(feature = "smarttags")]
            commands::smarttags_accept_suggestion,
            #[cfg(feature = "smarttags")]
            commands::smarttags_reject_suggestion,
            #[cfg(feature = "smarttags")]
            commands::smarttags_delete_index,
            #[cfg(feature = "smarttags")]
            commands::smarttags_train_classifiers,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
