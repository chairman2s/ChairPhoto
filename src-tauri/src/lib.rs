//! The Tauri shell: [`run`] builds the app, registers the command surface (`commands/`), the
//! native media protocols (`protocol.rs`) and the Tauri plugins, and wires the core's services
//! to the webview.
//!
//! Everything else lives in `chairphoto-core` (`crates/core`), re-exported here at the crate
//! root so `crate::catalog`, `crate::app` and the rest resolve in the shell exactly as they did
//! when both halves were one crate (issue #95). The core has no Tauri dependency; the shell
//! adds only what the webview needs.

pub use chairphoto_core::*;

mod commands;
mod protocol;
// The commands' unit tests use the same temp-dir fixture as the core's. `#[cfg(test)]` items
// are invisible across crates, so the shell compiles the core's file into itself rather than
// keeping a third copy (see that file's module docs for the other one, `tests/common`).
#[cfg(test)]
#[path = "../../crates/core/src/test_support.rs"]
mod test_support;

use commands::AppState;
use image_pool::ImageKind;
use protocol::handle_image_request;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // One tokio runtime for the process: Tauri's commands and `app::spawn_blocking` callers
    // outside a command (worker threads, the setup hook) share `app::runtime()`.
    tauri::async_runtime::set(app::runtime());
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

    // The Darkroom stage renders through `edit://<photoId>?r=<record>&m=<edge>` — the same
    // native path as the photo tiers below, never base64 over IPC (protocol.rs).
    #[cfg(feature = "edit")]
    let builder = builder.register_asynchronous_uri_scheme_protocol("edit", |ctx, req, responder| {
        protocol::handle_edit_request(ctx, req, responder);
    });

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
            // Every event the backend sends goes to the webview (commands' `EventSink`).
            // First, so nothing started below can send into the void.
            let state = app.state::<AppState>().inner().clone();
            state.set_events(std::sync::Arc::new(commands::WebviewEvents(app.handle().clone())));
            // The core's process startup — crash markers, upload sweep, Omarchy watcher,
            // decode analyzers and the image pool — shared with the GPUI app (app::boot).
            // `manage` holds the pool's Arc for the app's lifetime, which keeps its worker
            // threads alive; the URI scheme handlers find it there.
            let boot = crate::app::boot(&state);
            app.manage(boot.pool);

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

            match protocol::start_video_server(state.clone()) {
                Ok(port) => eprintln!("video server on http://127.0.0.1:{port}"),
                Err(e) => eprintln!("failed to start video server: {e}"),
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
            commands::get_system_theme,
            commands::list_external_modules,
            commands::get_modules_dir,
            #[cfg(feature = "module-fetch")]
            commands::module_fetch,
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
            commands::list_pending_identity,
            commands::summarize_pending_identity,
            commands::get_catalog_identity,
            commands::repair_pending_identity,
            commands::identity_repair_cancel,
            commands::identity_repair_status,
            commands::resolve_identity_conflict,
            commands::resolve_foreign_identity_conflicts,
            commands::identity_resolve_cancel,
            commands::identity_resolve_status,
            commands::list_owed_iptc,
            commands::dismiss_owed_iptc,
            commands::retry_owed_iptc,
            commands::find_unavailable_photos,
            commands::purge_unavailable_photos,
            commands::find_empty_photos,
            commands::purge_empty_photos,
            commands::vacuum_catalog,
            commands::video_server_port,
            commands::apply_offload_policy,
            commands::library_safety_summary,
            commands::trash_photos,
            commands::restore_photos,
            commands::list_trash,
            commands::empty_trash,
            commands::photo_safety_status,
            commands::scan_nas_folder_cmd,
            commands::list_pending_operations,
            commands::enqueue_operation,
            commands::enqueue_operations,
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
            commands::merge_tags,
            commands::split_tag,
            commands::find_orphan_tags,
            commands::find_similar_tags,
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
            commands::raw_probe,
            commands::develop_open,
            commands::develop_close,
            commands::develop_cache_usage,
            commands::develop_cache_clear,
            commands::develop_source,
            commands::render_edit,
            commands::render_edit_batch,
            commands::edit_zone_masses,
            commands::suggest_auto_tone,
            commands::list_luts,
            commands::import_lut,
            commands::delete_lut,
            commands::list_versions,
            commands::create_version,
            commands::rename_version,
            commands::set_version_edit,
            commands::version_history,
            commands::commit_version_edit,
            commands::goto_version_step,
            commands::set_cover_version,
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
            commands::available_editors,
            commands::develop_in_editor,
            commands::import_developed,
            commands::rapidraw_available,
            commands::edit_in_rapidraw,
            commands::cancel_rapidraw,
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
            commands::faces_match_status,
            #[cfg(feature = "faces")]
            commands::faces_match_cancel,
            #[cfg(feature = "faces")]
            commands::faces_accept,
            #[cfg(feature = "faces")]
            commands::faces_accept_person,
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
            commands::explain_photo_signals,
            commands::propose_stacks,
            commands::apply_stack_proposal,
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
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // A deliberate quit cuts in-flight decodes short; that is not a crash.
            if let tauri::RunEvent::Exit = event {
                crash_marker::clean_exit();
            }
        });
}
