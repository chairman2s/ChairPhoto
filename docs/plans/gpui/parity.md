# GPUI parity checklist

Every React component and TypeScript module in `src/` (tests and test stubs excluded), with
what it does for the user, the keys it handles, the backend commands and events it uses, the
GPUI ticket that ports it, and its status. A row is the acceptance list for its port. A
ported row becomes **ported** only after it has been checked in the running GPUI app with the
`chairphoto-app` skill.

Part of the map [Wayfinder: rewrite the GUI in GPUI][map] (ticket [Parity checklist of every
React component and module][t98]). The plan of record keeps this list in
`agent-notes/gpui-parity.md`. The ticket moved it here, into `docs/`, so every future
session can read it.

Inventory taken at `feature/gpui` `ebe3259`.

## How to read a row

The [inventory](#inventory) has one row per file, kept short so that an edit touches one
short line. Its columns:

- **React file** (the area tables' former "Component / module" column): the file in `src/`.
  The rest of its row is the file's section (`` ### `src/…` ``) under its area's heading
  below.
- **Area**: the area section that holds the file's section.
- **Tickets**: the tickets the status names as building the row, deciding its drop or
  porting it (#163, the visual check, is left out). The section's status says what each did.
- **GPUI path**: the port's main file or Rust path, relative to `crates/app/src/` unless it
  starts with `crates/` or is a Rust path. The section's status names the others; "—" for a
  dropped row.
- **Status**: one word (vocabulary since the [audit of 2026-10-03](#audit-2026-10-03)):
  - `built`: implemented, awaiting the visual check: not yet (or only partly) checked in the
    running app. A logic-only row (model crate, no UI of its own) is checked through the UI
    row that uses it.
  - `checked`: built and visually checked: the row is ported.
  - `partial`: specific listed behaviours are absent.
  - `dropped`: an owner decision or a reason recorded at inventory.
  - `to port`: nothing exists.

  A row can combine a built part with a dropped part; its word is that of the part its
  status names first.

Each file's section holds five paragraphs, one sentence or clause per line, wrapped at about
92 columns so that a change is a small diff:

- **Spec** (the inventory's former Features column): what the user sees and can do,
  including persisted settings (`key`), confirmations, pickers and clipboard use. For a
  logic file, the behaviour it implements.
- **Keys**: every key handled in that file and where the listener sits (window or element,
  bubble or **capture**). "—" means none.
- **Commands / events**: core commands as `wrapper`→`command` from `src/modules/api.ts`.
  Module commands are given as `api.invoke` names. Events are listed with the `ev:` prefix.
  Media protocols (`thumb://`, `preview://`, `zoom://`, `edit://`, the loopback video server)
  are named where used, because the [Image layer][t101] replaces them.
- **Port ticket**: the GPUI ticket that ports the row, by its short name (table below).
  Per-module ports that are not ticketed yet point at the map's "Not yet specified →
  Per-module ports" ([map notes][map]).
- **Status**, always last: the narrative behind the word, which it starts with:
  - `built (#N: path)`, awaiting the visual check.
  - `built and visually checked (#N: path)`, in practice written
    `built (#N: path), visually checked <date>`.
  - `partial (#N: path): missing X, Y`, written `partial (#N: path): … Missing: X, Y`: the
    absent behaviours follow "Missing:".
  - `dropped (decision: …)`.
  - `to port (#N)`.

The word and the narrative must agree. `shell::actions::stub_tickets` reads both: a
`not_yet_ported!` stub may cite only a ticket named after "Missing:" in a `partial` row's
Status paragraph, or anywhere in a `to port` row's. Its tests check that every row has a
section whose Status starts with its word (`checked` and `built` both start "built").

[map]: https://github.com/chairman2s/ChairPhoto/issues/92
[t96]: https://github.com/chairman2s/ChairPhoto/issues/96
[t97]: https://github.com/chairman2s/ChairPhoto/issues/97
[t98]: https://github.com/chairman2s/ChairPhoto/issues/98
[t99]: https://github.com/chairman2s/ChairPhoto/issues/99
[t100]: https://github.com/chairman2s/ChairPhoto/issues/100
[t101]: https://github.com/chairman2s/ChairPhoto/issues/101
[t102]: https://github.com/chairman2s/ChairPhoto/issues/102
[t103]: https://github.com/chairman2s/ChairPhoto/issues/103
[t104]: https://github.com/chairman2s/ChairPhoto/issues/104
[t105]: https://github.com/chairman2s/ChairPhoto/issues/105
[t106]: https://github.com/chairman2s/ChairPhoto/issues/106
[t107]: https://github.com/chairman2s/ChairPhoto/issues/107
[t108]: https://github.com/chairman2s/ChairPhoto/issues/108
[t109]: https://github.com/chairman2s/ChairPhoto/issues/109
[t110]: https://github.com/chairman2s/ChairPhoto/issues/110
[t111]: https://github.com/chairman2s/ChairPhoto/issues/111
[t112]: https://github.com/chairman2s/ChairPhoto/issues/112
[t113]: https://github.com/chairman2s/ChairPhoto/issues/113
[t114]: https://github.com/chairman2s/ChairPhoto/issues/114
[t115]: https://github.com/chairman2s/ChairPhoto/issues/115
[t116]: https://github.com/chairman2s/ChairPhoto/issues/116
[t117]: https://github.com/chairman2s/ChairPhoto/issues/117
[t120]: https://github.com/chairman2s/ChairPhoto/issues/120
[t121]: https://github.com/chairman2s/ChairPhoto/issues/121
[t151]: https://github.com/chairman2s/ChairPhoto/issues/151
[t157]: https://github.com/chairman2s/ChairPhoto/issues/157
[t158]: https://github.com/chairman2s/ChairPhoto/issues/158
[t161]: https://github.com/chairman2s/ChairPhoto/issues/161

Short names used in the Ticket column:

| Short name | Ticket |
|---|---|
| App crate | [GPUI app crate: window, core boot, event bridge][t99] |
| Deep links | [Single instance and chairphoto:// deep links without Tauri][t100] |
| Image layer | [Image layer: ImagePool to RenderImage with LRU and N±1 preload][t101] |
| Library logic | [Port library-side TS logic to Rust][t102] |
| Darkroom logic | [Port Darkroom-side TS logic to Rust][t103] |
| Module trait | [Module trait: contribution points for first-party Rust modules][t104] |
| Shell chrome | [Shell chrome: title bar, menu, command pill, bench, sidebar][t105] |
| Library view | [Library view: virtualised grid, thumbnails, filters][t106] |
| Tag panel | [Tag panel and tag editor][t107] |
| Photo inspector | [Photo inspector][t108] |
| Loupe | [Loupe, compare, cull, duel and proof sheet][t109] |
| Pop-out loupe | [Pop-out loupe window][t110] |
| Darkroom stage | [Darkroom stage and edit controls][t111] |
| Darkroom rails | [Darkroom rails: history, presets, lens, crop and versions][t112] |
| Preferences | [Preferences][t113] |
| Storage and import | [Storage and import: volumes, import, identity debt][t114] |
| Albums and export | [Albums, smart albums and export][t115] |
| Slippy map | [Slippy map for the Map module][t116] |
| Force layout | [Force layout for the Tag graph][t117] |
| Video | [Video playback in the GPUI app][t97] |
| Shell APIs | [GPUI shell APIs in gpui-kit 0.7][t96] |
| Per-module ports | Map notes, "Not yet specified → Per-module ports" ([map][map]) |

## Inventory

One row per file, in the order of the area sections below. The table holds only what
the stub guard and a reviewer scan for; each file's spec and status narrative are in its
section.

| React file | Area | Tickets | GPUI path | Status |
|---|---|---|---|---|
| `src/App.tsx` | Shell | #105, #106, #109, #114, #158–#160 | `view.rs` | built |
| `src/main.tsx` | Shell | #99, #110 | `assets.rs` | built |
| `src/components/Splash.tsx` | Shell | #160 | `shell/splash.rs` | built |
| `src/components/shell/TitleBar.tsx` | Shell | #105, #114, #148 | `shell/title_bar.rs` | checked |
| `src/components/shell/Menu.tsx` | Shell | #105 | `shell/title_bar.rs` | checked |
| `src/components/shell/CommandPill.tsx` | Shell | #105 | `shell/command_pill.rs` | checked |
| `src/components/shell/Bench.tsx` | Shell | #105, #114, #115 | `shell/bench.rs` | built |
| `src/components/shell/CollectionBrowser.tsx` | Shell | #105, #159 | `shell/sidebar.rs` | checked |
| `src/components/shell/IconRail.tsx` | Shell | #105, #111, #122 | `shell/sidebar.rs` | built |
| `src/components/shell/Inspector.tsx` | Shell | #105, #107 | `shell/inspector.rs` | built |
| `src/components/shell/index.ts` | Shell | — | — | dropped |
| `src/components/shell/railOrder.ts` | Shell | #105 | `shell/sidebar.rs` | built |
| `src/components/shell/useNarrow.ts` | Shell | #105 | `shell/state.rs` | built |
| `src/modules/shellTarget.ts` | Shell | #109, #110 | `shell/state.rs` | built |
| `src/modules/shellTiming.ts` | Shell | #102, #159 | `crates/model/src/shell_timing.rs` | built |
| `src/modules/labels.ts` | Shell | #105 | `shell/style.rs` | checked |
| `src/theme/tokens.ts` | Shell | #99, #102 | `crates/model/src/theme/tokens.rs` | built |
| `src/theme/standard.ts` | Shell | #99, #102 | `crates/model/src/theme/standard.rs` | built |
| `src/theme/omarchy.ts` | Shell | #99, #102 | `crates/model/src/theme/omarchy.rs` | built |
| `src/theme/controller.ts` | Shell | #99, #113 | `theme/mod.rs` | built |
| `src/theme/prefs.ts` | Shell | #113 | `machine_prefs.rs` | built |
| `src/theme/apply.ts` | Shell | — | — | dropped |
| `src/vite-env.d.ts` | Shell | — | — | dropped |
| `src/components/CatalogGrid.tsx` | Library | #106, #158 | `library/grid.rs` | checked |
| `src/components/Thumbnail.tsx` | Library | #101, #106 | `library/grid.rs` | built |
| `src/components/StackProposalsDialog.tsx` | Library | #106 | `library/stacks.rs` | built |
| `src/components/TrashDialog.tsx` | Library | #114 | `storage/trash.rs` | built |
| `src/modules/librarySession.ts` | Library | #102, #106 | `crates/model/src/library/session.rs` | checked |
| `src/modules/libraryQuery.ts` | Library | #102, #106 | `crates/model/src/library/query.rs` | built |
| `src/modules/previewCache.ts` | Library | #101, #109 | `image_store.rs` | built |
| `src/components/PhotoInspector.tsx` | Inspector and tags | #107, #108, #110, #159, #161 | `inspector/mod.rs` | built |
| `src/components/SignalsPanel.tsx` | Inspector and tags | #108 | `inspector/signals.rs` | checked |
| `src/components/IptcPanel.tsx` | Inspector and tags | #108, #148 | `inspector/mod.rs` | built |
| `src/components/MetadataPanel.tsx` | Inspector and tags | #108 | `inspector/render.rs` | built |
| `src/components/VersionsPanel.tsx` | Inspector and tags | #108, #111 | `inspector/render.rs` | checked |
| `src/components/PublishedPanel.tsx` | Inspector and tags | #108 | `inspector/render.rs` | checked |
| `src/components/PublishDialog.tsx` | Inspector and tags | #122, #123 | `modules/panel.rs` | checked |
| `src/components/TagPanel.tsx` | Inspector and tags | #107 | `tags/panel.rs` | built |
| `src/components/TagEditor.tsx` | Inspector and tags | #107 | `tags/editor.rs` | built |
| `src/components/TagCreateModal.tsx` | Inspector and tags | #107 | `tags/create.rs` | built |
| `src/components/TagMergeModal.tsx` | Inspector and tags | #107, #161 | `tags/merge.rs` | built |
| `src/components/TagSplitModal.tsx` | Inspector and tags | #107 | `tags/split.rs` | built |
| `src/components/TagGroupsManager.tsx` | Inspector and tags | #107 | `tags/groups.rs` | built |
| `src/modules/tagPaste.ts` | Inspector and tags | #102 | `crates/model/src/tag_paste.rs` | built |
| `src/components/ZoomableImage.tsx` | Loupe and cull | #109, #158 | `loupe/zoom.rs` | built |
| `src/components/PreviewImage.tsx` | Loupe and cull | #109 | `loupe/cull.rs` | built |
| `src/components/CompareView.tsx` | Loupe and cull | #109 | `loupe/compare_view.rs` | built |
| `src/components/CullSession.tsx` | Loupe and cull | #109 | `loupe/cull.rs` | built |
| `src/modules/compareDuel.ts` | Loupe and cull | #102 | `crates/model/src/compare_duel.rs` | built |
| `src/LoupeWindow.tsx` | Loupe and cull | #110 | `loupe/window.rs` | built |
| `src/components/LoupeCardView.tsx` | Loupe and cull | #110 | `loupe/card.rs` | built |
| `src/modules/loupe.ts` | Loupe and cull | #110 | `loupe/window.rs` | built |
| `src/components/darkroom/DarkroomView.tsx` | Darkroom | #111, #112 | `darkroom/view.rs` | built |
| `src/components/darkroom/DevelopSurface.tsx` | Darkroom | #111 | `darkroom/session.rs` | built |
| `src/components/darkroom/Filmstrip.tsx` | Darkroom | #111, #134 | `DarkroomView::render_filmstrip` | checked |
| `src/components/darkroom/filmstrip.ts` | Darkroom | #111, #134 | `chairphoto_model::darkroom::filmstrip` | built |
| `src/components/darkroom/ToneStrip.tsx` | Darkroom | #111 | `DarkroomView::render_tone_strip` | built |
| `src/components/EditControls.tsx` | Darkroom | #111, #112 | `darkroom/view.rs` | built |
| `src/components/darkroom/HistoryPanel.tsx` | Darkroom | #112 | `DarkroomView::render_history` | checked |
| `src/components/PresetBrowser.tsx` | Darkroom | #112 | `DarkroomView::render_presets` | checked |
| `src/components/darkroom/LensRail.tsx` | Darkroom | #112 | `DarkroomView::render_lens` | checked |
| `src/components/darkroom/RenderedImage.tsx` | Darkroom | #109, #112 | `loupe::duel::variant_image` | checked |
| `src/components/darkroom/ProofSheet.tsx` | Darkroom | #109, #112 | `loupe/proof_sheet.rs` | checked |
| `src/components/darkroom/DuelView.tsx` | Darkroom | #109, #112 | `loupe/duel.rs` | built |
| `src/components/darkroom/GlSpike.tsx` | Darkroom | #98 | — | dropped |
| `src/components/darkroom/developSource.ts` | Darkroom | #103 | `chairphoto_model::darkroom::develop_source` | built |
| `src/components/darkroom/history.ts` | Darkroom | #103 | `chairphoto_model::darkroom::history` | built |
| `src/components/darkroom/kelvin.ts` | Darkroom | #103 | `chairphoto_model::darkroom::kelvin` | built |
| `src/components/darkroom/spreads.ts` | Darkroom | #103 | `chairphoto_model::darkroom::spreads` | built |
| `src/components/darkroom/stageJson.ts` | Darkroom | #103 | `chairphoto_model::darkroom::stage_json::stage_json_for` | built |
| `src/components/darkroom/renderTiming.ts` | Darkroom | #111 | `chairphoto_model::darkroom::render_timing` | built |
| `src/modules/editing.ts` | Darkroom | #103, #111, #112 | `chairphoto_model::editing` | built |
| `src/modules/presets.ts` | Darkroom | #103, #112 | `chairphoto_model::presets` | built |
| `src/components/Preferences.tsx` | Preferences | #113, #161 | `preferences/` | checked |
| `src/components/SafetyPanel.tsx` | Preferences | #113 | `preferences/storage.rs` | checked |
| `src/components/ModulesPanel.tsx` | Preferences | #113, #122 | `modules/panel.rs` | checked |
| `src/components/ImportPanel.tsx` | Storage and import | #114 | `storage/import_panel.rs` | built |
| `src/components/VolumesPanel.tsx` | Storage and import | #113, #114 | `storage/volumes.rs` | checked |
| `src/components/IdentityDebtPanel.tsx` | Storage and import | #114 | `storage/identity_debt.rs` | built |
| `src/components/CatalogSwitcher.tsx` | Storage and import | #114 | `storage/catalog_switcher.rs` | checked |
| `src/components/BatchesPanel.tsx` | Storage and import | #114 | `storage/batches.rs` | checked |
| `src/components/BundleImportDialog.tsx` | Storage and import | #114 | `storage/bundle_import.rs` | built |
| `src/components/BundleExportDialog.tsx` | Storage and import | #115 | `export/bundle.rs` | built |
| `src/components/AlbumsPanel.tsx` | Albums and export | #115 | `albums/panel.rs` | checked |
| `src/components/SmartAlbumsPanel.tsx` | Albums and export | #115 | `albums/panel.rs` | built |
| `src/components/SmartAlbumEditor.tsx` | Albums and export | #115 | `albums/smart_editor.rs` | checked |
| `src/components/ExportPanel.tsx` | Albums and export | #115 | `export/panel.rs` | checked |
| `src/modules/plugins/aiTagging.tsx` | Bundled modules | #126 | `modules/ai_tagging/` | built |
| `src/modules/plugins/basicEditor.tsx` | Bundled modules | #104 | — | dropped |
| `src/modules/plugins/collage.tsx` | Bundled modules | #125 | `modules/collage/mod.rs` | checked |
| `src/modules/plugins/CollageDialog.tsx` | Bundled modules | #125 | `modules/collage/view.rs` | checked |
| `src/modules/plugins/collageTemplates.ts` | Bundled modules | #125 | `chairphoto_model::collage` | built |
| `src/modules/plugins/slideshow.tsx` | Bundled modules | #125 | `modules/slideshow/mod.rs` | checked |
| `src/modules/plugins/SlideshowDialog.tsx` | Bundled modules | #125 | `modules/slideshow/view.rs` | checked |
| `src/modules/plugins/localsend.tsx` | Bundled modules | #123 | `modules/localsend/mod.rs` | built |
| `src/modules/plugins/SendToDevicePanel.tsx` | Bundled modules | #123 | `modules/localsend/send.rs` | built |
| `src/modules/plugins/snapchat.tsx` | Bundled modules | #123 | `modules/localsend/mod.rs` | built |
| `src/modules/plugins/obsidian.tsx` | Bundled modules | #128 | `modules/obsidian/` | built |
| `src/modules/plugins/publishing.tsx` | Bundled modules | #123, #124 | `modules/publishing/` | built |
| `src/modules/plugins/flickr.tsx` | Bundled modules | #124 | `modules/flickr/` | built |
| `src/modules/plugins/smugmug.tsx` | Bundled modules | #124 | `modules/smugmug/` | built |
| `src/modules/plugins/instagram.tsx` | Bundled modules | #124 | `modules/instagram/` | built |
| `src/modules/plugins/faces.tsx` | Bundled modules | #129, #130 | `modules/faces/` | built |
| `src/modules/plugins/map.tsx` | Bundled modules | #119, #162 | `modules/map/` | built |
| `src/modules/plugins/smartTagging.tsx` | Bundled modules | #126 | `modules/smart_tagging/` | built |
| `src/modules/plugins/statistics.tsx` | Bundled modules | #127 | `modules/statistics/` | built |
| `src/modules/plugins/tagGraph.tsx` | Bundled modules | #110, #121 | `modules/tag_graph/` | checked |
| `src/modules/plugins/tagGraphBundle.ts` | Bundled modules | #121 | `chairphoto_model::tag_graph::bundle` | built |
| `src/modules/api.ts` | Module infrastructure and core API | #99, #104 | — | dropped |
| `src/modules/host.ts` | Module infrastructure and core API | #104, #122 | `modules/mod.rs` | built |
| `src/modules/registry.ts` | Module infrastructure and core API | #104, #122 | `modules/mod.rs` | built |
| `src/modules/ownedEvents.ts` | Module infrastructure and core API | #99 | `events.rs` | built |
| `src/modules/ModuleContent.tsx` | Module infrastructure and core API | #104 | — | dropped |
| `src/modules/bundled.ts` | Module infrastructure and core API | #122 | `modules/mod.rs` | built |

## Shell

### `src/App.tsx`

**Spec:** **Root shell.** Hosts Splash, TitleBar, IconRail (Library / Develop / module
views), the left CollectionBrowser and right Inspector columns (PhotoInspector +
QuickTagGroups).
Both columns drag-resize from 140 to 640 px.
CommandPill is hidden in Develop, module views and Compare.
**Stage** switches between DevelopSurface, a module main view, CompareView and the inline
loupe/CatalogGrid.
The grid empty-state text depends on the filter.
**Compare:** duel/grid modes, paged MAX_PANES at a time.
Keep = pick + reject the rest of the batch.
Duel verdicts reject the loser and pick the final winner.
**Inline loupe:** Back to grid;
Back to original (stacked child);
rotate ↺/↻ (non-destructive);
filename with pick/reject/★/soft/soft-in-burst/sharpest/version tags;
hint text.
Video items use a `<video>` player.
ZoomableImage shows the active-version render plus a hi-res zoom render.
When the photo is unavailable it offers Relocate… / Retrieve from NAS / Remove from catalog.
`loupe`-slot module panels.
**Bench:** progress (priority import > scan > develop), status line, marking, selection
pile.
**Grid context menu** (header with storage label): Move to trash (the selection if the
clicked tile is in it), Reveal in Files, Relocate… (file picker defaulting to library root),
Retrieve from NAS (disabled if local-only/missing), Remove from catalog (native confirm).
**Modals:** TagEditor, CatalogSwitcher, Preferences (can jump to a storage-tier filter),
IdentityDebtPanel, ImportPanel (background card import), ExportPanel, TrashDialog,
PublishDialog, bundle export/import, CullSession, StackProposalsDialog, module action modal,
TagGroupsManager.
**Other:** in-app tag copy/paste (not the OS clipboard);
assign/remove tag on the selection;
add selection to album;
back up the selection;
burst analysis;
rescan then cache previews (session-only toggle, default on).
Background NAS reconcile + offload policy on launch and on window focus.
Pending/identity-debt/trash counts.
Deep links `chairphoto://uuid[/loupe|/develop]` and `chairphoto://tag/uuid`.
Full state reset on catalog switch.
At ≤1024 px the panels become overlays behind a scrim. localStorage: `panel.leftW`,
`panel.rightW`, `panel.leftHidden`, `panel.rightHidden`, `panel.thumbSize` (120–320),
`panel.inspectorTab`, `panel.compareMode`.
Setting `sharpness.soft_threshold` (default 15).
Neighbour prefetch (+1..+5, −1, −2).
Syncs host selection, active version, filter context, editing tag, and the change and nav
sinks.

**Keys:** Window keydown (bubble).
Off in module views, Develop and cull sessions, and when the target is INPUT/TEXTAREA.
`[` toggles the left panel, `]` the inspector.
**Compare:** Esc/C exit.
Duel: ←/→ choose the winner.
Grid: PgDn/PgUp page;
←/↑ and →/↓ cycle pane focus.
K keeps the focused pane;
0–5 rate;
P/X/U pick/reject/unflag;
R/Y/G/B/V label, N clears.
**Grid:** C opens Compare (2+ selected);
Ctrl/Cmd+A selects all.
With an active photo: Enter toggles the loupe, Esc closes it;
→/↓ next and ←/↑ previous (Shift extends);
0–5, P/X/U, R/Y/G/B/V/N.
These apply to the whole selection and auto-advance only when one photo is targeted.
Modifiers are not checked (Ctrl+P also picks).
While the context menu is open, a second window keydown closes it on Esc.
Window `focus` runs reconcile and refreshes the trash count.

**Commands / events:** `initCatalog`→init_catalog, `applyAutoTags`→apply_auto_tags,
`videoServerPort`→video_server_port, `listRecentCatalogs`→list_recent_catalogs,
`listImportBatches`→list_import_batches, `getSetting`→get_setting, `listTags`→list_tags,
`rotatePhoto`→rotate_photo, `getPhotoByUuid`→get_photo_by_uuid,
`getLibraryRoot`→get_library_root, `pickFile` (dialog), `relocatePhoto`→relocate_photo,
`restorePhoto`→restore_photo, `removePhotoFromCatalog`→remove_photo_from_catalog (dialog
`confirm`), `addPhotosToAlbum`→add_photos_to_album, `assignTag`→assign_tag,
`removeTag`→remove_tag, `listPendingOperations`→list_pending_operations,
`summarizePendingIdentity`→summarize_pending_identity, `listTrash`→list_trash,
`reconcileNow`→reconcile_now, `applyOffloadPolicy`→apply_offload_policy,
`listVolumes`→list_volumes, `rescanLibrary`→rescan_library, `cacheImages`→cache_images,
`analyzeBurstSharpness`→analyze_burst_sharpness, `enqueueOperations`→enqueue_operations,
`ingestFromCard`→ingest_from_card_cmd, `setPickState`→set_pick_state,
`setRating`→set_rating, `setLabel`→set_label, `moveTag`→move_tag,
`setTagPrivate`→set_tag_private, `trashPhotos`→trash_photos, `revealPhoto`→photo_path +
opener `revealItemInDir`.
Via librarySession: list_photos, photo_statuses. ev: `cache:progress`, `scan:progress`,
`import:progress`, `develop:progress`, `catalog:switched`, `loupe:ready`.
Deep link `onOpenUrl`.
Emits `loupe:photo`;
opens the loupe `WebviewWindow`.
Host: initHost, setSelection, setHostActiveVersion, setFilterContext, setEditingTagContext,
setChangeSink, setNavSink, toolbarActionGroups, mainViews, panelsForSlot("loupe").

**Port ticket:** Shell chrome (layout, menus, modals wiring);
key bindings with Library view and Loupe;
deep links: Deep links;
state: Library logic

**Status:** built, partly visually checked 2026-10-03 (#163: the grid context menu, `[`/`]`,
the inline loupe, Compare's duel, the culling keys, rescan → cache warm-up);
since then the loupe's rotate chips are Lucide icons and its hint names "F faces" while
Faces is enabled (#172), and Compare's inspector header and bench marking follow the focused
pane (#170), both awaiting a visual re-check; the rest awaiting the visual check
(#105, #106, #109, #114, #158, #159, #160: `crates/app/src/view.rs`,
`crates/app/src/shell/state.rs`, `crates/app/src/library/grid.rs`,
`crates/app/src/library/grid_menu.rs`, `crates/app/src/library/photo_actions.rs`,
`crates/app/src/loupe/view.rs`, `crates/app/src/shell/layout_prefs.rs`,
`crates/app/src/model.rs`, `crates/app/src/storage/state.rs`): layout, column drag 140–640
px, narrow overlays, stage switching, Compare, inline loupe (rotate, Back to original, video
poster + "▶ Play in system player"), bench, modals, culling/Compare keys, reconcile on
launch and focus, deep links, catalog-switch reset, neighbour preload;
the **grid context menu** (#158: header with storage label, Move to trash, Reveal in Files,
Relocate…, Retrieve from NAS disabled without a backup, Remove from catalog behind a
confirm;
Esc and a click outside close it) and the loupe's unavailable-state Relocate… / Retrieve
from NAS / Remove from catalog (#158, the same commands).
Differences: a right-click on a tile that is part of the selection keeps the selection, so
Move to trash takes the selected photos the rows list (React selected the tile alone first,
so its "selection" branch never ran), and the row reads "Move N photos to trash" then;
a landed page of rows unselects photos it no longer lists (a filter hid them, or they were
trashed or removed), which React did not do;
Relocate…'s picker does not open at the library root (gpui's `PathPromptOptions` has no
starting folder).
Built since the audit by #159/#160: `apply_auto_tags` at startup (the boot chain, `model.rs`
`boot_after_open`);
a rescan's result starts the cache warm-up (`app::cache`, an owned job:
`JobRegistry::cache`, tripped by a newer warm-up or a catalog switch), with previews while
"Cache previews on import" is on, its job-scoped `cache:progress` on the bench's status line
("Caching N/M…") and its own result ending it ("Cache ready" / "Cache failed: …",
`storage/state.rs`);
the layout (`panel.leftW`/`rightW`/`leftHidden`/`rightHidden`/`thumbSize`/`inspectorTab`) is
restored from and written to `MachinePrefs` with React's keys and values
(`shell/layout_prefs.rs`;
a column width once per drag);
`[`/`]` are off in the Darkroom, module views and cull sessions: the keys dispatch key-only
`PanelKeyLeft`/`PanelKeyRight`, which the root gates by the surface shown, whatever has
focus (`view.rs` `panel_key`);
the View menu dispatches the toggles directly.

### `src/main.tsx`

**Spec:** Entry point.
Loads Instrument Sans (400/500/600/700) and Instrument Serif (400, 400 italic).
Applies the Standard palette before first paint.
Renders LoupeWindow when the hash is `#loupe`, otherwise App.

**Keys:** —

**Commands / events:** —

**Port ticket:** App crate (fonts via `AssetSource`, window routing)

**Status:** built (#99: `crates/app/src/assets.rs` `load_fonts`, Instrument Sans
400/500/600/700 + Serif 400/italic;
theme applied before the window opens;
#110: the pop-out loupe is its own window, `loupe::window`, replacing the `#loupe` hash
route), awaiting the visual check (font-weight matching still unverified, #99).

### `src/components/Splash.tsx`

**Spec:** Startup overlay: logo, "ChairPhoto", stepped progress bar over the boot stages
(Opening catalog… / Updating auto-tags… / Starting modules… / Loading photos…).
Shows "Ready", then fades out over 450 ms.

**Keys:** —

**Commands / events:** —

**Port ticket:** App crate (boot)

**Status:** built, awaiting the visual check (#160: `crates/app/src/shell/splash.rs`, driven
by `AppModel`'s boot): logo, name, stepped bar over the four stages (Opening catalog… →
Updating auto-tags… → Starting modules… on the first catalog read → Loading photos… until
the Library's first rows), "Ready", a 450 ms fade;
a failed open or row read ends it.
Only a launch that opens the default catalog shows it.

### `src/components/shell/TitleBar.tsx`

**Spec:** 46 px header.
Catalog pill (name · photo count, opens CatalogSwitcher).
Attention chips: "⤓ N waiting for the NAS" (runs reconcile) and "N identity debt" ("?" when
unknown).
**Import ▾** menu: Import from card…, Import a .chairphoto bundle…, Rescan library, "Cache
previews on import" checkbox, and an "Export a bundle" submenu of batches (or "No import
batches yet").
Export button (disabled without a selection).
**⋯ More** menu: Open loupe in a new window;
Loupe (badge "On");
Whole view group (Analyse burst sharpness, Propose stacks…, Start cull session);
Modules submenu (module toolbar actions grouped by module);
Identity debt (badge);
Back-up queue (badge);
View group (Tags & collections panel and Inspector checkboxes);
Preferences….

**Keys:** — (menu keys come from Menu)

**Commands / events:** — (presentational)

**Port ticket:** Shell chrome

**Status:** built (#105, #114, #148: `crates/app/src/shell/title_bar.rs`), visually checked
2026-10-03 (#163): catalog pill, NAS and identity-debt chips ("?" when unknown), Import ▾
with bundle-export submenu, Export, More ⋯ (pop-out, Loupe, whole-view group, Modules
submenu, badges, View checks, Preferences…).
Server-side decorations;
client-decorated fallback uses gpui-component's `TitleBar`.

### `src/components/shell/Menu.tsx`

**Spec:** Dropdown primitives.
MenuButton: trigger with aria-expanded;
left- or right-aligned panel;
a click or right-click outside closes it;
focus returns to the trigger.
MenuItem closes, then fires;
optional badge, danger style.
MenuCheckItem stays open.
MenuSub flies out on hover or click and flips left when right-aligned.
MenuSeparator, MenuLabel.

**Keys:** Element keydown while open: Esc closes;
↑/↓ move focus with wrap.
Every other key stops propagating, so an open menu swallows the culling keys.

**Commands / events:** —

**Port ticket:** Shell chrome

**Status:** built (#105: gpui-component `PopupMenu` in `shell/title_bar.rs`;
`[`/`]` muted in the `PopupMenu` context, `keymap.rs`), visually checked 2026-10-03 (#163).
Deliberate differences: the item's action runs before the menu closes, and a check item
closes the menu (React kept `MenuCheckItem` open).

### `src/components/shell/CommandPill.tsx`

**Spec:** Filter toolbar above the stage.
Culling segment All/Unrated/Picks/Rejects/Edited.
Colour-label dots (multi-select, OR) plus "No label".
Removable chips for Tag / Album / Smart album / Batch, facets, Camera, Lens and storage tier
(On disk / NAS only / At risk / Edits not carried home).
**＋ Filter** menu: Facets checklist;
Camera and Lens submenus;
Storage;
Sort (Date / Least sharp first / Sharpest first).
Thumbnail-size slider 120–320, step 8.

**Keys:** —

**Commands / events:** `listAlbums`→list_albums, `listSmartAlbums`→list_smart_albums,
`listFacets`→list_facets, `distinctPhotoValues`→distinct_photo_values,
`listImportBatches`→list_import_batches

**Port ticket:** Shell chrome (filter state with Library view)

**Status:** built (#105: `crates/app/src/shell/command_pill.rs`), visually checked
2026-10-03 (#163): culling segment, label dots + No label, scope/facet/camera/lens/storage
chips, ＋ Filter (Facets, Camera, Lens, Storage, Sort), thumb slider 120–320 step 8;
hidden outside the Library and in Compare.

### `src/components/shell/Bench.tsx`

**Spec:** 72 px bottom strip.
Progress readout (label and bar;
indeterminate shows 40 %), otherwise "N photos · M selected" plus the status line.
**Marking:** filename, 5 toggle stars (clicking the active star clears), Pick/Reject pills,
5 label dots and a clear dot.
**Selection:** "N on the table", first 3 thumbnails (active highlighted), then Compare
(needs 2+), Stack, Cull, Analyse, Export, Publish, Back up, ✕ Clear selection.

**Keys:** —

**Commands / events:** — (`thumb://` via Thumbnail)

**Port ticket:** Shell chrome

**Status:** built (#105, #114, #115: `crates/app/src/shell/bench.rs`), visually checked
2026-10-03 (#163); since then the pile shows its first three thumbnails with the marked one
ringed (#171) and in Compare the marking follows the focused pane (#170), both awaiting a
visual re-check: progress (import, export, scan,
develop) with Cancel for import/export, count + status line, marking via
`ShellState::apply_mark`, "N on the table" pile and its actions.

### `src/components/shell/CollectionBrowser.tsx`

**Spec:** Left column.
"library" header with All photos and Trash (count, opens TrashDialog).
Collapsible sections for tags / smart albums / albums / import batches, wrapping TagPanel,
SmartAlbumsPanel, AlbumsPanel and BatchesPanel.
Section state in localStorage `panel.section.{tags,smartAlbums,albums,batches}`.

**Keys:** —

**Commands / events:** —

**Port ticket:** Shell chrome

**Status:** built (#105, #159: `crates/app/src/shell/sidebar.rs`): All photos, Trash (count,
`OpenTrash`), collapsible tags / smart albums / albums / batches sections, module sidebar
slot — built, visually checked 2026-10-03 (#163).
`panel.section.*` restored from and written to `MachinePrefs` with React's values (#159,
`shell/layout_prefs.rs`).

### `src/components/shell/IconRail.tsx`

**Spec:** 52 px left rail: Library;
Develop (only when an editor module exists;
disabled with no selection);
one button per module main view (module icon, railOrder);
Preferences gear.
Active item highlighted.

**Keys:** —

**Commands / events:** —

**Port ticket:** Shell chrome

**Status:** built (#105, #111, #122: `crates/app/src/shell/sidebar.rs` `render_rail`),
visually checked 2026-10-03 (#163); the module icons (Map `map`, Stats
`chart-no-axes-column`, People `user-group`) are served since #173, awaiting a visual
re-check: Library, Develop (`edit`
feature, disabled with no selection), module main views in `rail_order`, Preferences gear.

### `src/components/shell/Inspector.tsx`

**Spec:** Right column.
Header: serif filename (path tooltip), colour-label swatch, hide button.
Tabs details / tags / versions / publish.
**QuickTagGroups** (tags tab): group chips including a virtual "Recently used" (10), "⚙
groups" (opens TagGroupsManager), and tag buttons that assign to the selection, plus empty
states.

**Keys:** —

**Commands / events:** `listTagGroups`→list_tag_groups,
`recentlyUsedTags`→recently_used_tags, `getGroupMembers`→get_group_members

**Port ticket:** Shell chrome (column, tabs);
QuickTagGroups with Tag panel

**Status:** built (#105, #107: `crates/app/src/shell/inspector.rs`;
QuickTagGroups in `crates/app/src/tags/photo_tags.rs` with "Recently used" (10) and "⚙
groups"), visually checked 2026-10-03 (#163), QuickTagGroups in a second pass
(its empty-state line ran off the column, #176, fixed in 1c44374: it now wraps inside the
row);
in Compare the header follows the selection, not the focused pane (#170).
Deliberate difference: the filename uses the UI sans, not the serif.

### `src/components/shell/index.ts`

**Spec:** Barrel re-export of the shell components.

**Keys:** —

**Commands / events:** —

**Port ticket:** Shell chrome

**Status:** dropped (decision: JS barrel file;
a Rust `mod` replaces it, `crates/app/src/shell/mod.rs`)

### `src/components/shell/railOrder.ts`

**Spec:** Rail order: map, people, statistics, tag-graph first (when present), then the rest
in input order.

**Keys:** —

**Commands / events:** —

**Port ticket:** Shell chrome

**Status:** built (#105: `crates/app/src/shell/sidebar.rs` `rail_order`, unit-tested),
awaiting the visual check.

### `src/components/shell/useNarrow.ts`

**Spec:** Hook: is the window ≤ 1024 px (matchMedia).
Drives the overlay-panel layout.

**Keys:** —

**Commands / events:** —

**Port ticket:** Shell chrome

**Status:** built (#105: `NARROW_MAX_W` in `shell/state.rs`, `ShellState::set_narrow`,
overlays behind a scrim in `view.rs`), awaiting the visual check.

### `src/modules/shellTarget.ts`

**Spec:** Resolves the photo that the inspector and pop-out loupe follow: Compare's focused
pane, else the selection.
The edit JSON is broadcast only when the broadcast id is the active selection, so one
photo's edit is never drawn on another.

**Keys:** —

**Commands / events:** —

**Port ticket:** Shell chrome (with Pop-out loupe)

**Status:** built (#109: `ShellState::loupe_target`;
#110: the inspector, inline loupe and pop-out follow it, a version renders only on its own
photo, `LoupeView::sync_version`), awaiting the visual check.

### `src/modules/shellTiming.ts`

**Spec:** Dev timing of the Develop→Library transition, gated by `editor.renderTiming`.
Grid commit, first/last tile load, 250 ms buckets, scroll, longest frame gap, marks, slow
invokes.
Summary saved to `editor.renderTiming.lastShell`.

**Keys:** —

**Commands / events:** `setSetting`→set_setting

**Port ticket:** Library logic (named in the ticket)

**Status:** built, awaiting the visual check (#102: `crates/model/src/shell_timing.rs`;
host #159: `crates/app/src/shell/timing.rs`): the Darkroom's `editor.renderTiming` read
enables it, ← Library starts a transition (started marker stored), the grid's first render,
tiles built/painted, the scroll to the selection and the row read (`list_photos`) are
recorded, and a 16 ms foreground ticker is the stall detector and quiet timer;
the summary is logged `[shell-timing]` and stored to `editor.renderTiming.lastShell` off the
UI thread in the rows' catalog.

### `src/modules/labels.ts`

**Spec:** Colour-label vocabulary and swatches: Red, Yellow, Green, Blue, Purple;
"" = none.

**Keys:** —

**Commands / events:** —

**Port ticket:** Library view

**Status:** built (#105: `COLOR_LABELS` in `crates/app/src/shell/style.rs`), visually
checked 2026-10-03 (#163).

### `src/theme/tokens.ts`

**Spec:** Token contract (19 names: canvas, panel, elev, well, border, line, txt, dim, mute,
accent, onaccent, sel, ok, onok, danger, rating, scrim, font-sans, font-display).
Payload types for the Omarchy palette and system theme.

**Keys:** —

**Commands / events:** —

**Port ticket:** App crate (theme builder)

**Status:** built (#102: `crates/model/src/theme/tokens.rs`;
consumed by `crates/app/src/theme/mod.rs`, #99), awaiting the visual check.

### `src/theme/standard.ts`

**Spec:** ChairPhoto Standard dark palette values and font stacks.

**Keys:** —

**Commands / events:** —

**Port ticket:** App crate (theme builder)

**Status:** built (#102: `crates/model/src/theme/standard.rs`;
#99: applied by `crates/app/src/theme/mod.rs`), awaiting the visual check.

### `src/theme/omarchy.ts`

**Spec:** Maps an Omarchy palette to tokens: derived surfaces by sRGB mixing, Standard
fallbacks, and a WCAG contrast guard (txt/dim 4.5, mute/accent 3.0) that steps toward white
or black.

**Keys:** —

**Commands / events:** —

**Port ticket:** Library logic (named in the ticket)

**Status:** built (#102: `crates/model/src/theme/omarchy.rs`, incl. the WCAG contrast guard;
the app uses it since `b0fea9f`, #99), awaiting the visual check.

### `src/theme/controller.ts`

**Spec:** Appearance policy.
Mode state with subscribers;
`setAppearanceMode` persists, repaints and notifies.
Follows live Omarchy theme switches only in follow mode.
No cross-window sync today (the loupe reads the mode on mount).

**Keys:** —

**Commands / events:** `getSystemTheme`→get_system_theme;
ev: `appearance:theme_changed`

**Port ticket:** App crate (live Omarchy refresh)

**Status:** built (#99, #113: `crates/app/src/theme/mod.rs` `Appearance` global,
`on_system_theme` routed from `CoreEvent::ThemeChanged` in `events.rs`;
tracks the system theme only in follow mode), awaiting the visual check.
Every window shares the theme, so the pop-out follows a mode change (React's did not).

### `src/theme/prefs.ts`

**Spec:** Persists the appearance mode in localStorage `appearance.mode` (`follow-omarchy`
default | `standard`).
The GPUI app needs a per-machine store, because this value is not in the catalog.

**Keys:** —

**Commands / events:** —

**Port ticket:** App crate (with Preferences → Appearance)

**Status:** built (#113: `crates/app/src/machine_prefs.rs` `MachinePrefs`, key
`appearance.mode`, default follow-omarchy), awaiting the visual check.

### `src/theme/apply.ts`

**Spec:** Writes the tokens as CSS custom properties on `:root`, plus `color-scheme` and
`data-appearance`.

**Keys:** —

**Commands / events:** —

**Port ticket:** App crate

**Status:** dropped (decision: CSS-variable plumbing;
the GPUI `Theme` builder replaces it, `crates/app/src/theme/mod.rs` `theme_config`)

### `src/vite-env.d.ts`

**Spec:** Vite client type reference.

**Keys:** —

**Commands / events:** —

**Port ticket:** Phase 6 cutover (map notes)

**Status:** dropped (decision: build tooling, deleted with Vite at the Phase 6 cutover)

## Library

### `src/components/CatalogGrid.tsx`

**Spec:** Virtualised photo grid (overscan 3 rows).
Column count from the thumb-size slider (`tileMin`, default 160), 3 px gap, 3:2 tiles.
Opens scrolled to the newest photo when nothing is selected, and scrolls the selection into
view.
Selected/active/rejected tile states.
Click selects (Ctrl/Cmd toggles, Shift selects a range), double-click opens, right-click
opens the context menu.
Badges: video ▶, stars, P/X, label dot, versions ⧉n, stack ▤n, soft `~` (below
`sharpness.soft_threshold`), `~B` soft in burst, ♛ sharpest of burst, storage (local ▣, NAS
☁, offline) with tooltips.
Filename.
Empty state "No photos.
Scan a folder to begin."
Reports the visible range for the status fetch.

**Keys:** Mouse modifiers only (keys live in App)

**Commands / events:** — (`thumb://` via Thumbnail)

**Port ticket:** Library view

**Status:** built (#106, #158: `crates/app/src/library/grid.rs` `LibraryView`,
`library/layout.rs`, `library/grid_menu.rs`), visually checked 2026-10-03 (#163);
Ctrl-click and double-click in a second pass; scrolling not exercised (eight photos fill
one row): virtualised `uniform_list` with
overscan 3, slider-driven columns, 3:2 tiles, opens at the newest photo, scrolls the active
one into view, selected/active/rejected states, click/Ctrl/Shift selection, double-click →
loupe, every badge with tooltips, filename, filter-worded empty state
(`layout::empty_message`), visible range reported;
right-click opens the **context menu** (#158;
its commands run in `library/photo_actions.rs` — see App.tsx's row for the two differences).

### `src/components/Thumbnail.tsx`

**Spec:** Lazy thumbnail.
Spinner placeholder until metadata is ready.
On error: "On NAS" ☁, "Missing" ⚠ or "No preview" ⚠, by storage status.
`bust` resets the failed state and cache-busts;
the cover token keys the image.

**Keys:** —

**Commands / events:** `thumb://`

**Port ticket:** Library view (images from Image layer)

**Status:** built (#106: `crates/app/src/library/grid.rs` `render_tile`;
images from #101 `ImageStore`): "…" placeholder until `metadata_ready`, On NAS ☁ / Missing ⚠
/ No preview ⚠ by status (`layout::failed_label`), rotation invalidates the tier
(`ImageStore::invalidate`) — the cover look keys the tile (#151:
`ImageStore::request_look_batch`;
a cover change re-renders only that tile) — awaiting the visual check.

### `src/components/StackProposalsDialog.tsx`

**Spec:** "Stack bursts" modal: "Looking for groups…", error, summary (groups/considered,
time gap, hamming threshold, already-stacked skipped, truncated).
Per group: frame count, time span, max visual distance, keeper reason or "chosen by hand".
Frame thumbs show ♛ keeper, sharpness, stars and ▤ child count;
click a frame to override the keeper.
Caveats for unscored frames and absorbed children.
"Stack N under this" / "Skip";
"Stacked N groups this session".

**Keys:** —

**Commands / events:** `proposeStacks`→propose_stacks,
`applyStackProposal`→apply_stack_proposal;
`thumb://`

**Port ticket:** Library view (scope addition on #106)

**Status:** built (#106: `crates/app/src/library/stacks.rs` `StackDialog`), awaiting the
visual check: looking/error/summary (time gap, hamming, already-stacked, truncated),
per-group frames/span/distance/keeper reason or "keeper chosen by hand", click-to-override
keeper, ♛/score/★/▤ per frame, unscored/absorbed caveats, "Stack N under this"/Skip,
"Stacked N groups this session";
Esc closes;
thumbnails windowed and released.

### `src/components/TrashDialog.tsx`

**Spec:** "Trash" modal: "N photos, most recently trashed first", selectable thumbnail grid.
Restore N/all (no confirm).
"Delete N/all permanently…" needs `delete` typed to confirm.
Report: deleted photos/files, skipped because a disk was unreachable, restored meanwhile,
"Stopped early", failures.
Footnote.

**Keys:** Confirm input: Enter deletes (only after typing `delete`), Esc cancels

**Commands / events:** `listTrash`→list_trash, `restorePhotos`→restore_photos,
`emptyTrash`→empty_trash;
`thumb://`

**Port ticket:** **No ticket names it** (closest: Storage and import)

**Status:** built (#114: `crates/app/src/storage/trash.rs` `TrashDialog`, from the
collection browser's Trash), awaiting the visual check: count line, selectable grid, Restore
N/all, "Delete N/all permanently…" behind typed `delete` (Enter only then, Esc cancels the
confirmation), report (deleted, unreachable, restored meanwhile, stopped early, failures).
Photos reach it through the grid context menu's Move to trash (#158).

### `src/modules/librarySession.ts`

**Spec:** `useLibrarySession`.
Four mutually exclusive scopes (tag / album / import batch / smart album) plus culling
filter, storage tier, facets, camera, lens, colour labels (OR;
"" = no label) and sort (default date).
`clearScope` keeps the sort.
Selection: plain, Ctrl-toggle, Shift-range from a pinned anchor;
`selectSingle`, `selectQuiet` (modules), `selectAll`, `stepActive(delta, extend)`,
`viewPhoto` (off-grid stack child + back to original), `clearSelection`.
`targets` = the selection or the active photo.
`reset` on catalog switch disowns in-flight requests.

**Keys:** —

**Commands / events:** via libraryQuery

**Port ticket:** Library logic

**Status:** built (#102: `crates/model/src/library/session.rs` `LibrarySession`, held by
`ShellState::library` in #106), visually checked 2026-10-03 (#163) through the grid: four
exclusive scopes, filter, tier, facets, camera, lens, labels, sort, `clear_scope` keeps the
sort;
select/Ctrl/Shift from an anchor, `select_single`, `select_quiet`, `select_all`,
`step_active`, `view_photo`/`back_to_original`, `clear_selection`, `reset` on switch.

### `src/modules/libraryQuery.ts`

**Spec:** `useLibraryQuery`: rows, total, storage-status map.
`refresh` carries a generation token (stale results dropped, old rows kept on error).
Storage status is fetched only for the visible window plus pinned ids, de-duplicated per
generation and retried on failure.

**Keys:** —

**Commands / events:** `listPhotos`→list_photos, `photoStatuses`→photo_statuses

**Port ticket:** Library logic

**Status:** built (#102: `crates/model/src/library/query.rs` `LibraryQuery`;
wired in #106 via `ShellState` `apply_page`/`apply_statuses`), awaiting the visual check:
generation-tagged refresh (stale dropped, old rows kept on error), statuses only for the
visible window plus pinned ids, de-duplicated per generation, failed ids askable again.

### `src/modules/previewCache.ts`

**Spec:** `previewUrl`/`zoomUrl` with `?v=bust`;
video URL over the loopback server;
`isVideoPath` (mp4/m4v/mov/avi/webm/mkv);
`prefetch` warms up to 60 previews.

**Keys:** —

**Commands / events:** `preview://`, `zoom://`, `http://127.0.0.1:{port}/{id}`

**Port ticket:** Image layer (video: Video)

**Status:** built (#101: `crates/app/src/image_store.rs` `ImageStore` over core
`ImageKind::{Thumb,Preview,Zoom}`;
preload windows #109 `navigate_window`), awaiting the visual check: preview/zoom tiers
replace the URLs, `invalidate` replaces `?v=bust`, N±k preload windows replace `prefetch`,
`chairphoto_core::scanner::is_video` replaces `isVideoPath`. dropped (decision: video =
poster + system player, #97): the loopback video URL — the loupe shows the poster and "▶
Play in system player" (`loupe/view.rs`).

## Inspector and tags

### `src/components/PhotoInspector.tsx`

**Spec:** "Select a photo" when there is none.
**details:** EXIF line (camera · lens · ƒ · shutter · ISO);
1–5 stars (click the current star to clear);
Pick/Reject/None;
5 label swatches + Clear;
Culling signals (SignalsPanel).
Collapsible sections, state in localStorage `inspector.section.<id>`: **Stack** (master +
children with thumbs;
View opens in the loupe;
Unstack);
**Orientation** (↺ −90, ↻ +90, 180°);
**Edit in** (darktable/RawTherapee/ART;
Import result when a CLI exists;
RapidRAW with an editing/waiting/importing note and Cancel);
**Storage** (status;
Back up / Offload local / Restore local;
queues a backup when the NAS is offline);
**IPTC**;
**Metadata**.
**tags:** chips with × (removes from the whole selection);
Copy tags;
"Paste N → M";
add-tag input with autocomplete (top 8, deepest match first, excluding assigned tags), Enter
creates the path when nothing matches;
"From nearby photos" chips with a ±30 s–10 min window (`nearby_window_seconds`);
`inspector`-slot module panels.
**versions:** VersionsPanel.
**publish:** PublishedPanel + "Publish…".

**Keys:** Add-tag input: ↑/↓ move the highlight, Enter picks or creates, Esc clears

**Commands / events:** `getSetting`/`setSetting`→get_setting/set_setting,
`getPhotoTags`→get_photo_tags, `suggestTagsByTime`→suggest_tags_by_time,
`createTag`→create_tag, `setRating`→set_rating, `setPickState`→set_pick_state,
`setLabel`→set_label, `getPhoto`→get_photo, `listStackChildren`→list_stack_children,
`unstackPhoto`→unstack_photo, `availableEditors`→available_editors,
`developInEditor`→develop_in_editor, `importDeveloped`→import_developed,
`rapidrawAvailable`→rapidraw_available, `editInRapidraw`→edit_in_rapidraw,
`cancelRapidraw`→cancel_rapidraw, `backupPhoto`→backup_photo,
`enqueueOperation`→enqueue_operation, `offloadPhoto`→offload_photo,
`restorePhoto`→restore_photo;
ev: `rapidraw:progress` (one global listener, keyed by photo);
`thumb://`

**Port ticket:** Photo inspector

**Status:** built, details and tags tabs visually checked 2026-10-03 (#163),
the rest awaiting the visual check (#108: `crates/app/src/inspector/mod.rs`, `inspector/render.rs`;
tags tab = #107's `tags/photo_tags.rs` + inspector-slot module panels in `view.rs`): section
state `inspector.section.<id>` per machine (#159, `MachinePrefs`, default collapsed);
details (EXIF line, stars, Pick/Reject/None, labels, signals), Stack (View →
`ShellState::view_in_loupe`, Unstack), Orientation, Edit in (incl.
Import result and RapidRAW with Cancel;
the list re-checks when Preferences → Editors saves a setting, #161 `model::EditorsChanged`
→ `PhotoInspector::reread_editors`), Storage, IPTC, Metadata;
tags tab (chips, Copy/Paste, add-tag autocomplete, nearby window, quick tags);
follows Compare's focused pane (`ShellState::loupe_target`, #110)

### `src/components/SignalsPanel.tsx`

**Spec:** Read-only "Culling signals" (reloads per photo, ignores stale results).
Sharpness score vs threshold, Soft/Above verdict, method and its note.
Burst verdict, rank, cluster size, split-from group, median/cutoff/soft %, sharpest frame,
stale and truncation warnings.
Frames table (sharpness, Δ hamming, ♛/~) with "Showing x of y".
Other badges: stack children, stacked-under parent, version count.

**Keys:** —

**Commands / events:** `explainPhotoSignals`→explain_photo_signals

**Port ticket:** Photo inspector

**Status:** built (#108: `crates/app/src/inspector/signals.rs`), visually checked 2026-10-03
(#163)

### `src/components/IptcPanel.tsx`

**Spec:** Caption/Description, Headline, Title, Creator, Copyright, Credit, Source, City,
State/Province, Country, Country code.
"Save IPTC" is enabled only when dirty.
Status "Saving…" / "Saved to sidecar" / "Saved to catalog;
sidecar pending (…)" (#148) / "Failed: …".
Reloads per photo.

**Keys:** —

**Commands / events:** `getIptc`→get_iptc, `setIptc`→set_iptc

**Port ticket:** Photo inspector

**Status:** built (#108: `inspector/mod.rs` `IptcForm` / `save_iptc`;
the pending-sidecar status from #148, core `app::iptc` `IptcSaveOutcome::status`), awaiting
the visual check.
Saves are serialised per photo;
a queued save survives navigating away and reports on the status line

### `src/components/MetadataPanel.tsx`

**Spec:** Read-only stored EXIF/IPTC/XMP grouped by family, with collapsible headers and
counts.
EXIF, Composite, IPTC and XMP start open;
the rest start collapsed.
"No metadata".

**Keys:** —

**Commands / events:** `getPhotoMetadata`→get_photo_metadata

**Port ticket:** Photo inspector

**Status:** built (#108: `inspector/render.rs` `render_metadata`, `META_DEFAULT_OPEN`),
awaiting the visual check

### `src/components/VersionsPanel.tsx`

**Spec:** "Original" + named versions;
the active one is highlighted;
click selects it for the loupe;
double-click renames inline.
Per version: ✎ Edit (only with an editor module), ⧉ Duplicate, ✕ Delete (no confirm;
falls back to Original).
"New version" input + "+ Add" (default "Version N").
Hint to enable Basic Editor.

**Keys:** Rename: Enter commits, Esc cancels.
New-version input: Enter adds.

**Commands / events:** `listVersions`→list_versions, `createVersion`→create_version,
`renameVersion`→rename_version, `duplicateVersion`→duplicate_version,
`deleteVersion`→delete_version

**Port ticket:** Photo inspector

**Status:** built (#108: `inspector/render.rs` `render_versions`;
✎ opens the Darkroom, #111), visually checked 2026-10-03 (#163).
The "enable Basic Editor" hint: dropped (decision #104: basicEditor folds into the Darkroom;
✎ shows when the `edit` feature is compiled in)
Seen 2026-10-03: Original + Version 1 with ✎/⧉/✕ and the New version field.

### `src/components/PublishedPanel.tsx`

**Spec:** Publications (platform · version or "Original" · date) with ✕ (no confirm);
"Not published yet".
"Mark as published": platform input (suggestions instagram/flickr/smugmug), version select
(default = active version), "+ Mark".

**Keys:** Platform input: Enter records

**Commands / events:** `listPublications`→list_publications, `listVersions`→list_versions,
`recordPublication`→record_publication, `deletePublication`→delete_publication

**Port ticket:** Photo inspector

**Status:** built (#108: `inspector/render.rs` `render_publish`, `COMMON_PLATFORMS`),
visually checked 2026-10-03 (#163)
Seen 2026-10-03: "Not published yet", Mark as published with platform chips, Publish….

### `src/components/PublishDialog.tsx`

**Spec:** "Publish" modal.
One chip per module publish target;
renders the chosen target's form.
Empty state points to Preferences → Modules.
Close/backdrop.

**Keys:** —

**Commands / events:** — (targets from host `publishTargets`)

**Port ticket:** Photo inspector (targets via Module trait)

**Status:** built (#122, #123: `crates/app/src/modules/panel.rs` `open_publish_dialog` /
`PublishDialog`, targets from the Module trait), visually checked 2026-10-03 (#163)
Seen 2026-10-03: target chips (Instagram, Device (LocalSend), Snapchat) and the Instagram
form; the LocalSend/Snapchat chips not chosen (choosing one starts LAN discovery); nothing
posted.

### `src/components/TagPanel.tsx`

**Spec:** Tag tree: header with ＋ New tags… and Expand/Collapse all.
"Search tags…" (top 8 with counts;
a pick selects the tag, expands its ancestors and scrolls to it).
"All photos" clears the filter.
Rows: twisty, name, 🔒 private, "auto", count, ⚙ edit;
click filters the grid.
**Drag-and-drop** reparents a tag (onto "All photos" = top level).
Context menu: Move to…, Move to top level, Make private/public (and incl. sub-tags), Merge
into…, Split off N selected…, Edit…, New child tags….
Move modal with filter and ↑ Top level.
Results go to the status line.

**Keys:** Search: ↑/↓, Enter picks, Esc clears.
Window keydown while the context menu is open: Esc closes.

**Commands / events:** — (parent callbacks: move_tag, set_tag_private;
child modals)

**Port ticket:** Tag panel

**Status:** built (#107: `crates/app/src/tags/panel.rs`, `tags/move_tag.rs`), awaiting the
visual check.
Difference: a search pick scrolls its row to the top of the browser, not centred (GPUI
`ScrollAnchor`)

### `src/components/TagEditor.tsx`

**Spec:** Modal.
Inline rename (commits on blur/Enter;
reverts on error).
Path subtitle.
Delete → "Confirm delete".
Description (saved on blur).
"Organizational — don't export".
Translations (list with ×, add lang + text).
Synonyms (per-synonym export checkbox, ×, add).
Export preview with language checkboxes and live chips.
`tag-editor`-slot module sections.

**Keys:** Rename: Enter commits.
Translation and synonym inputs: Enter adds.

**Commands / events:** `listTagTerms`→list_tag_terms, `listLanguages`→list_languages,
`getTagExportable`→get_tag_exportable, `setTagExportable`→set_tag_exportable,
`tagExportPreview`→tag_export_preview, `addTagTerm`→add_tag_term,
`removeTagTerm`→remove_tag_term, `setTermExport`→set_term_export, `renameTag`→rename_tag,
`setTagDescription`→set_tag_description, `deleteTag`→delete_tag

**Port ticket:** Tag panel

**Status:** built (#107: `crates/app/src/tags/editor.rs`, with the tag-editor module slot),
awaiting the visual check

### `src/components/TagCreateModal.tsx`

**Spec:** "New tags" (optionally under a parent).
Multi-line input (indented hierarchy or a/b paths);
live preview capped at 30 rows;
"N tags total";
"Create N tags" creates them in order and stops at the first error.

**Keys:** Window keydown: Esc closes

**Commands / events:** `createTag`→create_tag

**Port ticket:** Tag panel

**Status:** built (#107: `crates/app/src/tags/create.rs`), awaiting the visual check

### `src/components/TagMergeModal.tsx`

**Spec:** Two steps.
Pick the target from a filtered list (excluding itself and its subtree, max 200).
Then a dry-run preview: photos gained, collapsed assignments, children repathed, terms,
synonyms, quick-tag groups, smart-album rules, faces, face rejections, classifiers dropped,
suggestions, tombstones, skipped terms, warnings.
Merge only after the preview;
"Pick a different tag".
Also opened from Preferences → Tags.

**Keys:** —

**Commands / events:** `mergeTags`→merge_tags (dry run, then commit)

**Port ticket:** Tag panel

**Status:** built (#107: `crates/app/src/tags/merge.rs`, opened from the Tag panel;
#161: also from Preferences → Tags, `preferences/tags.rs` `OpenTagMerge` →
`preferences/mod.rs` `Preferences::open_tag_merge`), awaiting the visual check

### `src/components/TagSplitModal.tsx`

**Spec:** "Split <name>" for the selected photos.
New path input;
"Keep <name> on these photos too";
dry-run preview (gain, lose, already had, never carried);
Split only after the preview.

**Keys:** Path input: Enter previews

**Commands / events:** `splitTag`→split_tag

**Port ticket:** Tag panel

**Status:** built (#107: `crates/app/src/tags/split.rs`), awaiting the visual check

### `src/components/TagGroupsManager.tsx`

**Spec:** "Tag groups" modal: group chips, new group.
For the active group: member chips (×), delete group (no confirm), add member by path
(created if new), rename on blur.
The quick-tag bar refetches on close.

**Keys:** New group and add member: Enter adds

**Commands / events:** `listTagGroups`→list_tag_groups, `createTagGroup`→create_tag_group,
`renameTagGroup`→rename_tag_group, `deleteTagGroup`→delete_tag_group,
`getGroupMembers`→get_group_members, `addTagToGroup`→add_tag_to_group,
`removeTagFromGroup`→remove_tag_from_group

**Port ticket:** Tag panel

**Status:** built (#107: `crates/app/src/tags/groups.rs`;
the quick-tag block re-reads on every write), awaiting the visual check
(its empty dialog was seen 2026-10-03, #163)

### `src/modules/tagPaste.ts`

**Spec:** `parseTagPaste`: pasted lines → full tag paths.
Indentation builds the hierarchy (tab = 4, space = 1).
Lines containing `/` or `|` are kept verbatim.
De-duplicated, order preserved.

**Keys:** —

**Commands / events:** —

**Port ticket:** Library logic

**Status:** built (#102: `crates/model/src/tag_paste.rs`, used by `tags/create.rs`);
logic only, no visual check needed

## Loupe and cull

### `src/components/ZoomableImage.tsx`

**Spec:** Wheel zoom toward the cursor (×1.15;
max 100 %, or 8× until hi-res loads;
overrides floor at 4×).
Drag pans when zoomed.
Double-click toggles fit ↔ 100 % at the cursor.
Swaps preview → full-res on the first zoom-in.
"Fit N%" button.
`srcOverride` shows an edited version's render and fetches a hi-res override lazily.
Controllable view, so Compare panes share one.
Unavailable state with action buttons.
`bust` re-fetches.

**Keys:** —

**Commands / events:** `preview://`, `zoom://`

**Port ticket:** Loupe

**Status:** built (#109, #158: `crates/app/src/loupe/zoom.rs` `ZoomImage` / `ZoomShared` /
`ZoomView`), awaiting the visual check
(wheel zoom toward the cursor and Fit N% seen 2026-10-03, #163):
wheel/drag/double-click zoom, the tier swap, Fit N%,
the override's hi-res, the shared view, and the unavailable state's Relocate…, Retrieve from
NAS and Remove from catalog (#158: `RelocatePhoto` / `RetrieveFromNas` /
`RemoveFromCatalog`, run by the root view on the inline loupe's photo,
`library/photo_actions.rs`)

### `src/components/PreviewImage.tsx`

**Spec:** Plain preview: "No photo selected", "No preview available".
Used only by CullSession.

**Keys:** —

**Commands / events:** `preview://`

**Port ticket:** Loupe

**Status:** built (#109: folded into `crates/app/src/loupe/cull.rs`, "No preview
available"), visually checked 2026-10-03 (#163);
the frame is cropped instead of fitted (#174)

### `src/components/CompareView.tsx`

**Spec:** 2–4 panes sharing one pan/zoom, reset when the set changes.
Bar: "‹ Back to grid (Esc)";
Duel/Grid chips when the pool is > 2;
paging ‹ › "Comparing a–b of N";
"Duel r of n" or the champion text;
"Fit N%";
"⚠ mixed sizes".
Pane header: name, champion/♛ tag, stars, pick/reject, label, sharpness ⌖ (soft-coloured), ♛
sharpest.
Mouse-down focuses a pane.
Footer "Keep this" / "This one wins".
Empty "Nothing to compare… press C".

**Keys:** None in this file (App handles Esc, ←/→, 0–5, P/X, K, U, PgUp/PgDn)

**Commands / events:** — (mode in localStorage `panel.compareMode`)

**Port ticket:** Loupe

**Status:** built (#109: `crates/app/src/loupe/compare_view.rs`, `loupe/compare.rs`;
mode in machine prefs `panel.compareMode`), visually checked 2026-10-03
(#163: Duel and Grid mode; paging not exercised;
a portrait frame in a narrow Grid pane is cropped, #175).
Difference: no "Nothing to compare" state — Compare ends when no pane is left
(`shell/state.rs:926-929`)

### `src/components/CullSession.tsx`

**Spec:** Full-screen, keyboard-only cull over a frozen list.
Resumes at `cull.cursor.photo_id` (saved 400 ms debounced, and on finish) with a note.
HUDs: position, filename, stars, PICK/REJECT, label, "End of set", "Not saved — …" (the
decision is rolled back), progress bar.
Prefetches 5 ahead and 1 behind.
Each decision auto-advances with an optimistic write, no wrap.
Help overlay.
Summary: visited, remaining, decided, picked, rejected, rated, labelled, time and s/photo,
"Back to the grid".

**Keys:** Window keydown (ignores inputs): Esc closes help, else ends → summary;
Esc/Enter on the summary exit;
h/? help;
→/↓/Space next;
←/↑ back;
0–5 rate;
p/x/u;
r/y/g/b/v label, n clears (all advance)

**Commands / events:** `getSetting`/`setSetting`→get_setting/set_setting,
`setRating`→set_rating, `setPickState`→set_pick_state, `setLabel`→set_label;
`preview://` prefetch

**Port ticket:** Loupe

**Status:** built (#109: `crates/app/src/loupe/cull.rs` `CullState` / `CullView`;
keys in `loupe/mod.rs`, CULL context), visually checked 2026-10-03 (#163):
HUD, help, rating with advance, summary; the frame is cropped instead of fitted (#174)

### `src/modules/compareDuel.ts`

**Spec:** Champion/challenger tournament over a frozen pool.
`advanceDuel(left/right)` returns the next state, the loser (the caller rejects it) and, on
the last round, the winner (the caller picks it).
N frames take N−1 rounds.

**Keys:** —

**Commands / events:** —

**Port ticket:** Library logic (named in the ticket)

**Status:** built (#102: `crates/model/src/compare_duel.rs`, driven by `loupe/compare.rs`);
logic only, no visual check needed

### `src/LoupeWindow.tsx`

**Spec:** Pop-out loupe root: "No photo selected", a module LoupeCard (takes over while
set), or ZoomableImage of the broadcast photo with the active version's render and hi-res
zoom.
Face overlay when `faces` is compiled in.
Follows appearance.

**Keys:** —

**Commands / events:** `pluginFeatures`→plugin_features;
`renderForLoupe` (render_edit or `edit://`);
face overlay shim: list_tags, get_setting `faces.*`, raw invoke;
ev: `loupe:photo`, `loupe:card`;
emits `loupe:ready`;
get_system_theme / `appearance:theme_changed`

**Port ticket:** Pop-out loupe

**Status:** built (#110: `crates/app/src/loupe/window.rs` `LoupeWindowView`, a `LoupeView`
with `Follow::Window`), awaiting the visual check: "No photo selected", the target with its
version render and hi-res zoom, a module card while one is up, the Darkroom's print (#112);
loupe-slot panels per window (the faces overlay, #129);
the theme is the app's, shared
Not checked: needs the owner (window rule).

### `src/components/LoupeCardView.tsx`

**Spec:** Module card in the pop-out loupe: colour dot, title, subtitle, chips, stats,
"Connected" chips;
photo wall ("N photos") paged 48 at a time ("Show more (N left)");
a tile opens full-size with "← {title}" back.

**Keys:** Window keydown while viewing a photo: Esc back to the wall

**Commands / events:** `listPhotos`→list_photos;
`thumb://`, `preview://`/`zoom://`

**Port ticket:** Pop-out loupe

**Status:** built (#110: `crates/app/src/loupe/card.rs` `CardView`), awaiting the visual
check: dot, title, subtitle, chips, stats, CONNECTED;
"N photos" wall paged 48 at a time ("Show more (N left)");
a tile opens full-size with "← {title}" and Esc back
Not checked: needs the owner (window rule).

### `src/modules/loupe.ts`

**Spec:** Opens or focuses the `loupe` window (1280×800);
broadcasts photo + edit JSON + render source, and module cards;
ready handshake makes the main window resend.

**Keys:** —

**Commands / events:** ev: `loupe:photo`, `loupe:card`, `loupe:ready`;
`WebviewWindow`

**Port ticket:** Pop-out loupe (shared entities replace the events)

**Status:** built (#110: `crates/app/src/loupe/window.rs` `open`), awaiting the visual
check: one 1280×800 window, opened or raised from More ⋯ or a module;
shared entities replace the events and the ready handshake
Not checked: needs the owner (window rule).

## Darkroom

### `src/components/darkroom/DarkroomView.tsx`

**Spec:** **Top bar:** ← Library;
"Darkroom";
version shelf chips (Original + versions;
switching saves first);
"saving…"/"rendering…";
source badge.
For an engine-1 version of a RAW: warn badge + "Develop with the new engine" (forks "<name>
(RAW)", keeps geometry).
"◩ Clipping" (RAW) overlay.
"🖥 Loupe print" (`basic-editor.printOnLoupe`, default on;
opens the loupe).
"☆ Use as cover"/"★ Cover".
"+ New version" (forks, named after the last adopted proof or "Version N").
Error banner.
**Stage** = EditStage;
ToneStrip below.
**Actions:** "▦ Deal a proof sheet" (after auto-tone) → ProofSheet;
"⚖ Refine by duel" → DuelView;
"☆ Save as preset" (inline name, "Saved preset" notice);
Reset.
Filmstrip when > 1 photo.
**Right rail:** HistoryPanel, ToneRail (Kelvin when the RAW has an as-shot WB;
`develop.wbSlider`), PresetBrowser, EffectsRail, LensRail (RAW with a lens table),
GeometryRail (`editor.crop_overlay`).
**Autosave** 600 ms after changes settle, each a named history step (amend window);
waits while the RAW prepares;
saves on unmount, step, switch and cover.
The first change on Original creates "Version N".
**Render tiers:** fast 720 px (≈90 ms throttle), settled 1400 px after 250 ms, plus zone
masses and the loupe broadcast.
Crop is an overlay;
perspective is un-warped while its handles are up.
Dev frame timing (`editor.renderTiming`, `editor.renderTiming.lastSummary`).
Preloads neighbours.

**Keys:** Window keydown with Ctrl/Meta: Ctrl+S saves now (even in text fields);
Ctrl+Z undo;
Ctrl+Shift+Z / Ctrl+Y redo (not while typing).
Preset name: Enter saves, Esc cancels.

**Commands / events:** `listVersions`→list_versions, `versionHistory`→version_history,
`developOpen`→develop_open, `getSetting`/`setSetting`, `suggestAutoTone`→suggest_auto_tone,
`createVersion`→create_version, `setVersionEdit`→set_version_edit,
`commitVersionEdit`→commit_version_edit, `gotoVersionStep`→goto_version_step,
`setCoverVersion`→set_cover_version, `editZoneMasses`→edit_zone_masses;
`edit://` (`editRenderUrl`, `k=1` clip layer);
ev: `develop:source`, `loupe:ready`;
emits `loupe:photo`;
presets via settings

**Port ticket:** Darkroom stage (stage, tiers, source badge, clipping, filmstrip);
Darkroom rails (history, presets, versions, cover, proof/duel entry points)

**Status:** built (#111, #112: `crates/app/src/darkroom/view.rs`, `darkroom/view/rails.rs`,
`darkroom/session.rs`, `darkroom/session/rails.rs`), awaiting the visual check.
Every listed behaviour found: bar (← Library, saving…/rendering…, source badge, ◩ Clipping,
🖥 Loupe print, ☆ Use as cover/★ Cover, + New version, Develop with the new engine, error
banner), version shelf, ToneStrip, proof sheet/duel/Save as preset/Reset, filmstrip, rail
(history, WB K/± with Kelvin, presets, effects, lens, geometry), autosave 600 ms
(`AUTOSAVE_QUIET`) as named steps, tiers 720/1400 px + 250 ms settle (`darkroom/stage.rs`),
zone masses, neighbour preload via `develop_open`, Ctrl+S/Z/Shift+Z/Y.
Deliberate differences recorded in #112 (changes refused while a step/switch/fork is on the
worker;
undo with a pending change lands on H;
the print is taken down on step/leave/switch).
Visual check 2026-10-03 (#163): bar (Original/Version chips, rendering…, RAW badge, ◩
Clipping toggle, Use as cover, + New version), version shelf, ToneStrip, actions, filmstrip
and the whole rail seen on a RAW; 🖥 Loupe print not clicked (it opens the pop-out loupe,
which needs the owner: window rule); Develop with the new engine and the error banner not
reached.

### `src/components/darkroom/DevelopSurface.tsx`

**Spec:** Outlives each photo.
Mounts DarkroomView keyed by photo, so per-photo state never crosses photos.
On leaving Develop it releases the develop session and refreshes the Library.
Preloaded neighbours survive stepping.

**Keys:** —

**Commands / events:** `developClose`→develop_close

**Port ticket:** Darkroom stage

**Status:** built (#111: `darkroom::Darkroom` in `crates/app/src/darkroom/session.rs`),
awaiting the visual check: outlives each photo, one `OpenPhoto` per photo;
`leave` calls `develop_close` and re-reads the Library rows.

### `src/components/darkroom/Filmstrip.tsx`

**Spec:** Library-order strip, ±40 around the current photo;
current frame centred and highlighted;
click moves (the Darkroom saves first);
"<name> (i of N)";
cover looks.

**Keys:** Window keydown: ←/→ previous/next (no wrap).
Ignored with modifiers, while a proof sheet or duel is open, or in inputs.

**Commands / events:** `thumb://`

**Port ticket:** Darkroom stage

**Status:** built (#111, #134: `DarkroomView::render_filmstrip` / `centre_strip` in
`crates/app/src/darkroom/view.rs`), visually checked 2026-10-03 (#163): ±40 window, current
highlighted and centred, "<name> (i of N)", click steps (saves first), ←/→ guarded against
modifiers, inputs and the proof/duel overlay, cover looks via `ImageStore::request_looks`.
Seen 2026-10-03: the current frame highlighted and → stepping seen (eight frames do not
overflow the strip, so centring was not exercised).

### `src/components/darkroom/filmstrip.ts`

**Spec:** `windowAround` (radius 40), `stepTarget` (no wrap), `arrowsBelongToTarget` (inputs
keep their arrows).

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom stage (not in the Darkroom logic ticket's list)

**Status:** built (#111, #134: `chairphoto_model::darkroom::filmstrip` — `window_around`,
`step_target`, `arrows_belong_to_target`, `centred_scroll`);
logic, seen through the Filmstrip row.

### `src/components/darkroom/ToneStrip.tsx`

**Spec:** Adjustable histogram of 8 EV zones, fill = normalised pixel mass.
Drag a zone up/down to offset it (60 px/EV, ±2 EV);
delta label;
double-click resets.

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom stage

**Status:** built (#111: `DarkroomView::render_tone_strip` +
`chairphoto_model::darkroom::tone_strip`), awaiting the visual check: 8 zones filled by
mass, drag ±2 EV at 60 px/EV, delta label, double-click resets.
Visual check 2026-10-03 (#163): the 8 zones fill by mass and refill per photo; dragging not
exercised.

### `src/components/EditControls.tsx`

**Spec:** **EditStage:** fit stage;
wheel zoom toward the cursor (1–8×);
drag pans;
"Fit NN%".
Crop box: move, 4 corner handles (aspect-locked, min 5 %), "W × H px".
Guides None/Thirds/Phi grid/Golden spiral.
Perspective quad with 4 handles.
Straighten: draw a line (> 8 px) to level.
Clipping layer slot.
(`showBefore` Before/After exists but is never passed: dead.)
**ToneRail:** WB with a K/± mode button;
Kelvin (log slider) + Tint ±50, double-click returns to as-shot;
relative temp/tint;
Exposure ±3, Contrast, Highlights, Shadows, Whites, Blacks;
Vibrance, Saturation;
double-click resets.
**EffectsRail:** Color / B&W Neutral/Red/Yellow/Green;
Fade, Vignette, Grain, Grain size;
Split toning (shadow/highlight hue and sat, balance);
LUT (.cube) select with "(missing)", "Import…" file picker, amount.
**GeometryRail:** aspects Original, Free, 1:1, 4:5, 1.91:1, 9:16, 16:9, 3:2, 2:3, 4:3, 3:4;
overlay chips (`editor.crop_overlay`);
"Output W × H";
Perspective Correct/Adjust/Done + Reset;
Straighten "Draw level line", Reset, angle ±45°.

**Keys:** EditStage window keydown (not in inputs): Enter zooms to the crop;
Esc resets the view to fit

**Commands / events:** `listLuts`→list_luts, `importLut`→import_lut, `pickFile` (dialog),
`setSetting`→set_setting

**Port ticket:** Darkroom stage (EditStage, ToneRail, EffectsRail, LUT);
Darkroom rails (GeometryRail: crop, rotate, perspective, overlays)

**Status:** built (#111, #112: EditStage in `darkroom/view.rs`, overlays and GeometryRail in
`darkroom/view/rails.rs`, control tables in
`chairphoto_model::darkroom::{controls, geometry}`), awaiting the visual check: wheel zoom
1–8× toward the cursor, pan, "Fit NN%", Enter zoom-to-crop, Esc fit;
crop box with 4 corner handles and "W × H px";
guides None/Thirds/Phi/Golden spiral;
perspective quad Correct/Adjust/Done + Reset;
level line (≥ 8 px, `LEVEL_MIN_PX`) and angle slider with Reset;
ToneRail incl.
K/± and Kelvin/Tint;
EffectsRail incl.
B&W filters, split toning, LUT with "(missing)", Import…, amount;
all 11 aspects;
Output W × H.
The dead `showBefore` is not ported (by design).
Visual check 2026-10-03 (#163): ToneRail (Kelvin/Tint with K field), EffectsRail (B&W
filters, Fade, Vignette, Grain, Split toning, LUT list with Import…), Crop & Rotate aspects,
Overlay guides, Output W × H, Perspective and Straighten rails seen; stage zoom, crop
handles and the level line not exercised. A long LUT name overflowed the rail (#180); since
1c44374 the chip is cut to the rail with an ellipsis and shows the full name in a tooltip.
Difference: React picks a LUT from a dropdown, which has no tooltip.

### `src/components/darkroom/HistoryPanel.tsx`

**Spec:** "History" with the Ctrl+Z hint;
steps newest first with label and relative time;
current marked, undone steps styled;
click goes to a step;
empty-state text.

**Keys:** — (keys in DarkroomView)

**Commands / events:** — (goto_version_step via callback)

**Port ticket:** Darkroom rails

**Status:** built (#112: `DarkroomView::render_history` in
`crates/app/src/darkroom/view/rails.rs`), visually checked 2026-10-03 (#163): newest first, label +
relative time, current/undone styling, click → `goto_step`, Ctrl+Z hint, empty state.
Seen 2026-10-03: newest-first steps with dates and the empty state seen; click-to-step not
exercised.

### `src/components/PresetBrowser.tsx`

**Spec:** Collapsible "Presets" by category (Monochrome, Film, Color, User).
Each card is a 320 px render of the current photo with that preset (lazy on expand).
Active card highlighted.
Click applies (replaces tone + look, flattens zones, keeps framing).
User presets: ✎ rename modal, × delete (no confirm).

**Keys:** Rename: Enter confirms, Esc cancels

**Commands / events:** `get_setting`/`set_setting` (`basic-editor.presets`);
`edit://…&m=320`

**Port ticket:** Darkroom rails

**Status:** built (#112: `DarkroomView::render_presets` in
`crates/app/src/darkroom/view/rails.rs`), visually checked 2026-10-03 (#163): collapsible, four
categories, 320 px renders while open, active highlight, apply, ✎ rename (inline field
rather than a modal;
Enter/Esc), × delete.
Seen 2026-10-03: four categories rendered at 320 px; apply, rename and delete not exercised.

### `src/components/darkroom/LensRail.tsx`

**Spec:** "Lens": Correction on/off chip (`lens.builtin`);
hint of what the file's tables fix (vignetting, distortion, CA) and their source.

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom rails

**Status:** built (#112: `DarkroomView::render_lens` +
`chairphoto_model::darkroom::lens_rail`), visually checked 2026-10-03 (#163): Correction on/off on
the record's `lens.builtin`, hint of what the tables fix and their source.
Seen 2026-10-03: "Correction off" with the Sony built-in tables hint.

### `src/components/darkroom/RenderedImage.tsx`

**Spec:** An `edit://` render with a loading placeholder and "—" on error.
Used by presets, proof sheet and duel.

**Keys:** —

**Commands / events:** `edit://`

**Port ticket:** Darkroom rails (images from Image layer)

**Status:** built (#109, #112: `loupe::duel::variant_image` over
`loupe::edit_renders::EditRenders`), visually checked 2026-10-03 (#163);
serves presets, proof sheet and duel.
A failed render shows "—", as React did (#161: `variant_placeholder`).
Seen 2026-10-03: through the preset tiles and the proof sheet.

### `src/components/darkroom/ProofSheet.tsx`

**Spec:** Modal "your photo, developed N ways": 320 px proofs with label and group;
as-shot marked current;
click adopts;
✕/backdrop close.

**Keys:** Window keydown, **capture**: Esc closes

**Commands / events:** `edit://`

**Port ticket:** Loupe (entry point in Darkroom rails)

**Status:** built (#109, #112: `crates/app/src/loupe/proof_sheet.rs`, opened by
`DarkroomView::open_proof_sheet`), visually checked 2026-10-03 (#163): 320 px proofs with
label/group, as-shot marked current, click adopts ("Proof: X"), ✕/backdrop/Esc close under
its own `ProofSheet` key context.
Deliberate keyboard difference recorded in #112 (Tab cycles, Enter/Space adopt the focused
proof).
Seen 2026-10-03: 12 proofs with labels and groups, As shot current, ✕/Esc close; adopting
not exercised.

### `src/components/darkroom/DuelView.tsx`

**Spec:** "⚖ Duel — round n" with dimension pills;
two 1024 px variants ("Rendering…");
click or "← This one"/"This one →" applies and advances;
closes after the last dimension;
⑂ forks the variant as a version ("Kept as …");
hint "↓ same · Esc done".

**Keys:** Window keydown, **capture** (stop + prevent): ← left, → right, ↓ same, Esc close

**Commands / events:** `edit://`

**Port ticket:** Loupe (entry point in Darkroom rails)

**Status:** built (#109, #112: `crates/app/src/loupe/duel.rs`, opened by
`DarkroomView::open_duel`), awaiting the visual check: "⚖ Duel — round n" with dimension
pills, two 1024 px variants, click or ← This one / This one →, ⑂ fork ("Kept as …"), "↓ same
· Esc done";
keys ←/→/↓/Esc under the `Duel` key context.
Visual check 2026-10-03 (#163): bar, dimension pills and Esc match; the variants are cropped
and the buttons sit over them (#178).

### `src/components/darkroom/GlSpike.tsx`

**Spec:** Dev-only WebGL probe modal, reached from Preferences → Editors → "Run WebGL probe"
when `editor.renderTiming` is on.

**Keys:** Window keydown, capture: Esc

**Commands / events:** —

**Port ticket:** —

**Status:** dropped (decision: dev-only WebGL probe;
the production Darkroom has no WebGL — #98;
noted in `crates/app/src/preferences/mod.rs`)

### `src/components/darkroom/developSource.ts`

**Spec:** `badgeFor` source badges (camera preview / preparing, RAW · N-bit · X MP,
unsupported, JPEG, no decoder);
`isPreparing`;
`reduceSource` folds `develop:source` events for this photo (RAW → token, engine 2,
cameraEv, as-shot WB, lens).

**Keys:** —

**Commands / events:** payload of `develop:source` / develop_open

**Port ticket:** Darkroom logic

**Status:** built (#103: `chairphoto_model::darkroom::develop_source` — `badge_for`,
`is_preparing`, `reduce_source`);
logic, seen through the DarkroomView source badge.

### `src/components/darkroom/history.ts`

**Spec:** `describeChange` names a change ("Exposure +0.50", "White balance 5500 K", "Crop
4:5", "LUT x", …;
several = "Adjustments");
`shouldAmend` (same key, at the tip, within 4000 ms).

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom logic

**Status:** built (#103: `chairphoto_model::darkroom::history` — `describe_change`,
`should_amend`, `AMEND_WINDOW_MS` 4000);
logic, seen through the History rail.

### `src/components/darkroom/kelvin.ts`

**Spec:** `develop.wbSlider` (kelvin | relative);
2000–12000 K log slider (50 K steps);
tint ±50;
mired shifts (proof ±30, duel 25);
`wbShown`.

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom logic

**Status:** built (#103: `chairphoto_model::darkroom::kelvin` — 2000–12000 K log slider,
tint, `mired_shift`, `wb_shown`, `WB_SLIDER_KEY`);
logic, seen through the ToneRail.

### `src/components/darkroom/spreads.ts`

**Spec:** Proof spread of 12 (current/as shot, auto, auto warm/cool, preset looks
round-robin).
Duel dimensions ev/warmth/contrast/shadows, halving per revisit;
`duelPair`.

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom logic

**Status:** built (#103: `chairphoto_model::darkroom::spreads` — `proof_spread`, `DuelDim`,
`duel_pair`);
logic, seen through the proof sheet and duel.

### `src/components/darkroom/stageJson.ts`

**Spec:** Stage record JSON without the crop, and without perspective while it is being
edited.

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom logic

**Status:** built (#103: `chairphoto_model::darkroom::stage_json::stage_json_for`);
logic, seen through the stage.

### `src/components/darkroom/renderTiming.ts`

**Spec:** Dev render timing: `editor.renderTiming`, `editor.renderTiming.lastSummary`;
percentiles;
tiers;
summary;
`[edit-timing]` log.

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom stage (drag-frame measurement)

**Status:** built (#111: `chairphoto_model::darkroom::render_timing`;
the stage stamps every frame in `darkroom/stage.rs`;
`examples/darkroom_bench.rs`);
dev logic, no visual check of its own.

### `src/modules/editing.ts`

**Spec:** VersionEdit record: crop, tone (incl.
WB modes), perspective quad, straighten, bw, split, grain, fade, vignette, lut, zones[8],
engine, display, cameraEv, lens.
Engine-2 helpers, look helpers, BW filters, straighten clamp, `levelFromLine`,
`inscribedCrop`, aspects, overlays, golden spiral, default quad, tolerant `parseEdit`,
`fitCrop`.

**Keys:** —

**Commands / events:** —

**Port ticket:** Darkroom logic

**Status:** built (#103, #111, #112: `chairphoto_model::editing` — VersionEdit record,
`parse_edit`, `level_from_line`, `inscribed_crop`, `fit_crop`, aspects, overlays, golden
spiral — plus `chairphoto_model::darkroom::{controls, geometry}`);
logic, seen through the Darkroom rows.

### `src/modules/presets.ts`

**Spec:** 17 built-in presets (Monochrome, Film, Color);
user presets as JSON under `basic-editor.presets`;
`lookOnly`, `addUserPreset`, `allPresets`.

**Keys:** —

**Commands / events:** `getSetting`/`setSetting`

**Port ticket:** Darkroom logic

**Status:** built (#103, #112: `chairphoto_model::presets` — `builtin_presets`,
`parse_user_presets`/`serialize_user_presets` under `basic-editor.presets`, `look_only`,
`add/rename/delete_user_preset`, `all_presets`);
logic, seen through the PresetBrowser.

## Preferences

### `src/components/Preferences.tsx`

**Spec:** Modal, tabs Storage, Tags, Editors, Modules, Appearance, plus one per enabled
module with settings.
**Storage:** library root input + Set ("re-scan to index it");
Volumes;
Safety;
Local/NAS tiering (`offload_age_days`, Save, "Offload older now");
"Index existing NAS photos" (path, Browse… defaulting to the backup volume, Index, status);
Maintenance: remove unavailable / remove empty (0-byte) photos (find, then native confirm
listing up to 12 paths), "Compact database now" (before → after MB).
**Tags:** Tidy redundant tags;
Find duplicate tags (≤ 50 pairs, "Merge X away…" → TagMergeModal);
Find unused tags (branch/auto badges, Delete;
confirm only for a branch with children).
**Editors:** darktable/RawTherapee/ART rows (GUI/CLI found ✓/✗, paths `editor.<key>.gui` /
`.cli`);
RapidRAW (found, `editor.rapidraw.bin`, `editor.rapidraw.format` tiff/png/jpg);
Darkroom: decode cache GB (`develop.decodeCacheGb`, default 20) with "X used" + Clear,
"Prepare the next and previous photo…" (`develop.preloadNeighbours`), WB for new edits
(`develop.wbSlider`), export-parity line (`metrics.exportParity`), dev "Log render timings"
(`editor.renderTiming`) with "Last drag summary" (`editor.renderTiming.lastSummary`).
**Modules:** ModulesSection.
**Appearance:** Follow Omarchy / ChairPhoto Standard (localStorage `appearance.mode`) with a
live theme status line.
**Module tabs:** each module's settings panels, or "This module has no settings."

**Keys:** NAS folder input: Enter indexes.
Decode-cache input: Enter saves.

**Commands / events:** `getLibraryRoot`/`setLibraryRoot`→get/set_library_root,
`getSetting`/`setSetting`, `applyOffloadPolicy`→apply_offload_policy,
`listVolumes`→list_volumes, `pickFolder` (dialog), `scanNasFolder`→scan_nas_folder_cmd,
`findUnavailablePhotos`/`purgeUnavailablePhotos`→find/purge_unavailable_photos,
`findEmptyPhotos`/`purgeEmptyPhotos`→find/purge_empty_photos,
`vacuumCatalog`→vacuum_catalog, `tidyRedundantTags`→tidy_redundant_tags,
`listTags`→list_tags, `findSimilarTags`→find_similar_tags,
`findOrphanTags`→find_orphan_tags, `deleteTag`→delete_tag,
`availableEditors`→available_editors, `rapidrawAvailable`→rapidraw_available,
`developCacheUsage`/`developCacheClear`→develop_cache_usage/clear,
`getSystemTheme`→get_system_theme;
ev: `appearance:theme_changed`;
dialog `confirm`

**Port ticket:** Preferences (module tabs via Module trait)

**Status:** built (#113: `crates/app/src/preferences/` — `storage.rs`, `tags.rs`,
`editors.rs`, `appearance.rs`, `mod.rs`;
#161: "Merge X away…"), visually checked 2026-10-03 (#163): all five tabs plus one per enabled module
("This module has no settings.");
library root Set, Volumes, Safety, tiering (Save, Offload older now), Index existing NAS
photos (Browse…, Enter indexes), Maintenance (confirm listing paths, Compact before →
after), Tidy/duplicates/unused (branch/auto badges, branch confirm), editor rows and keys,
RapidRAW, decode cache (Enter/blur saves, Clear), preload, WB, export parity, render timing +
last summary, Appearance per machine, "Merge X away…" opening the Tag panel's merge preview
over Preferences (live only while its tag is in the tag tree of the section's catalog;
the section reports the merge with the Tag panel's `merge_summary` wording). dropped
(decision: dev-only probe): GlSpike "Run WebGL probe" / `editor.glSpike.lastReport`.
Seen 2026-10-03: every tab and the per-module tabs (AI Tagging, Obsidian, Map, Faces, Smart
Tagging); Storage's Library, Volumes, Safety, tiering, Index existing NAS photos,
Maintenance, Compact; Tags' three sections; Editors incl. RapidRAW and the Darkroom block;
Appearance switching Standard ↔ Omarchy; no action button run. Checkbox labels were
oversized (#180); since 1c44374 every checkbox uses `ui::checkbox` with a 12 px label (13 px
in the Darkroom block). The label colour is still gpui-component's foreground, not React's
dimmer `--dim`; the Slideshow's text toggles (11.5 px) are unchanged.

### `src/components/SafetyPanel.tsx`

**Spec:** Preferences → Storage → Safety: counts At risk (oldest waiting age), Edits not
carried home, Unverified, Safe, No copy anywhere;
"Show me" on At risk/Stale filters the grid by storage tier;
next steps and caveats.

**Keys:** —

**Commands / events:** `librarySafetySummary`→library_safety_summary

**Port ticket:** Preferences

**Status:** built (#113: `crates/app/src/preferences/storage.rs` safety section), awaiting
the visual check: At risk (oldest waiting), Edits not carried home, Unverified, Safe, No
copy anywhere;
"Show me" on At risk/Stale sets the storage-tier filter and closes Preferences.
Visually checked 2026-10-03 (#163): the four buckets with counts and Show me.

### `src/components/ModulesPanel.tsx`

**Spec:** Preferences → Modules: "Modules" (bundled) and "Installed modules" (external).
Row: name, version, "external" badge, description, "Requires:" (unmet marked), "backend not
included in this build", blocked reason, "enabled" checkbox.
Backend-access and network-access lists and the PermissionReview modal ("Allow and enable").
Install hint with the modules directory.
`modules.enabled`, `modules.permissions`.

**Keys:** —

**Commands / events:** `getModulesDir`→get_modules_dir;
host enable/disable/grantPermissions/listModules (set_setting)

**Port ticket:** Preferences (list, enable, requires, backend availability via Module trait)

**Status:** built (#122: `crates/app/src/modules/panel.rs` `ModulesPanel`, Preferences →
Modules in #113), visually checked 2026-10-03 (#163): one row per compiled-in module with name,
description, "Requires:" (unmet marked), "backend … not included in this build", blocked
reason, enabled checkbox;
`modules.enabled`. dropped (decision: modules compiled-in, third-party JS dropped — #104):
Installed/external modules, "external" badge, version, permission/network review,
modules-directory hint, `modules.permissions`.
Seen 2026-10-03: rows with descriptions, "Requires: LocalSend", "backend … not included in
this build" for Flickr/SmugMug, enable toggles adding settings tabs.

## Storage and import

### `src/components/ImportPanel.tsx`

**Spec:** "Import from card": source input, Browse… (scans automatically), Scan.
Result: "N photos · N new · N already imported · N selected", Select all new / Select all /
Clear, thumbnail tiles with ✓ and "dup" (new ones pre-selected).
"No photos found".
Optional import name.
Destination note (library root, Year/Month/Day).
"Import N" hands off to the background import.
Last source in `import.lastSource`.

**Keys:** Source input: Enter scans

**Commands / events:** `getSetting`/`setSetting`, `getLibraryRoot`→get_library_root,
`listCardPhotos`→list_card_photos_cmd, `cardThumbnail`→card_thumbnail (data URL, limited to
6 at once), `pickFolder` (dialog);
the import itself is App's ingest_from_card_cmd + `import:progress`

**Port ticket:** Storage and import

**Status:** built (#114: `crates/app/src/storage/import_panel.rs` `ImportPanel`), awaiting
the visual check: source + Browse… (scans at once) + Scan, Enter scans, counts line, Select
all new / Select all / Clear, tiles with ✓ and dup (new pre-selected), "No photos found",
import name, destination note, "Import N" hands off to the bench-owned background import;
`import.lastSource`;
card thumbnails at most 6 at once.

### `src/components/VolumesPanel.tsx`

**Spec:** Preferences → Storage → Volumes: name, kind ("NAS / backup" / "local"),
reachable/offline, base path.
Remove (confirm) on all but the library folder.
Add: name, base path (`~` expands), kind.

**Keys:** Base path: Enter adds

**Commands / events:** `listVolumes`→list_volumes, `addVolume`→add_volume,
`removeVolume`→remove_volume;
`window.confirm`

**Port ticket:** Storage and import

**Status:** built (#114: `crates/app/src/storage/volumes.rs` `VolumesPanel`, mounted in
Preferences → Storage by #113), visually checked 2026-10-03 (#163): name, kind, reachable/offline,
base path;
Remove behind a confirm except the library folder;
Add with `~` expansion and kind;
Enter adds.
Seen 2026-10-03: the library-folder volume and the Add row; Add/Remove not exercised.

### `src/components/IdentityDebtPanel.tsx`

**Spec:** "Identity debt" modal: summary (copies, conflicts, dismissed);
explanation of Unreachable/Unwritable/Conflict/Adopt/Overwrite/Dismiss.
"Start repair pass" background job with "Repairing… X of Y" and Cancel;
re-attaches to a running pass on reopen;
finished/stopped summary.
"Show dismissed (N)".
Separate error lines.
Virtualised table: State, Path, Volume, Relative path, Field, Tries, Last attempt, Detail,
Resolve (Adopt, Overwrite… with inline confirm, Dismiss, Restore). 500-row pages with
Prev/Next/"Back to first page".

**Keys:** —

**Commands / events:** `summarizePendingIdentity`→summarize_pending_identity,
`listPendingIdentity`→list_pending_identity, `listVolumes`→list_volumes,
`repairPendingIdentity`→repair_pending_identity,
`cancelIdentityRepair`→identity_repair_cancel,
`identityRepairStatus`→identity_repair_status,
`resolveIdentityConflict`→resolve_identity_conflict;
ev: `identity:repair_progress` (job-filtered), `identity:repair_done` (required terminal;
buffered;
fails closed without a listener)

**Port ticket:** Storage and import

**Status:** built (#114: `crates/app/src/storage/identity_debt.rs`), awaiting the visual
check: summary, explanation, "Start repair pass" job with "Repairing… X of Y" and Cancel,
re-attach on reopen, finished/stopped summary, "Show dismissed (N)", error lines, columns
State/Path/Volume/Relative path/Field/Tries/Last attempt/Detail/Resolve (Adopt, Overwrite…
inline confirm, Dismiss, Restore), 500-row pages with Prev/Next/"Back to first page".
Virtualised since #162: the queue is a variable-height virtual list (one 22 px line per owed
field, React's `rowHeight`) and the owed-IPTC list a `uniform_list`;
only the rows on screen are built, and each row's buttons are built with it.
Since #169 every Resolve button captures its copy and the catalog it was read from when it is
drawn, and a catalog switch re-reads the queue; since #164 the React panel is catalog-bound
too and closes on a switch (GPUI re-reads instead).
Difference: the Resolve buttons stay on one line (no wrap, a 240 px column), as React's
fixed row height assumed.
Visual check 2026-10-03 (#163): summary, explanation, Start repair pass and the empty state
seen; no debt in the isolated catalog, so the table and repair job were not exercised.

### `src/components/CatalogSwitcher.tsx`

**Spec:** "Open catalog": recent catalogs (name, path, relative last-opened);
click switches.
"New catalog": name, folder + Browse…, "Creates <name>.chairphoto inside the folder",
Create.
Busy state;
error.

**Keys:** —

**Commands / events:** `listRecentCatalogs`→list_recent_catalogs,
`switchCatalog`→switch_catalog, `pickFolder` (dialog);
App handles `catalog:switched`

**Port ticket:** Storage and import

**Status:** built (#114: `crates/app/src/storage/catalog_switcher.rs` `CatalogSwitcher`, the
title bar's catalog pill), visually checked 2026-10-03 (#163): recent catalogs (name, path, relative
last opened;
click switches), New catalog (name, folder + Browse…, note, Create), busy and error;
the folder expands `~` (React created a literal `~`).
Seen 2026-10-03: recent catalogs and New catalog; switching and creating not exercised.

### `src/components/BatchesPanel.tsx`

**Spec:** Sidebar "Import batches": source label and count;
click toggles the batch filter;
⬇ "Export as bundle" per row;
"No imports yet".

**Keys:** —

**Commands / events:** `listImportBatches`→list_import_batches

**Port ticket:** Storage and import

**Status:** built (#114: `crates/app/src/storage/batches.rs` `RootView::render_batches`),
visually checked 2026-10-03 (#163): label and count, click toggles the batch filter, ⬇ "Export as
bundle" per row, "No imports yet".
Seen 2026-10-03: label, count and ⬇ export button in the sidebar; the bundle export not run.

### `src/components/BundleImportDialog.tsx`

**Spec:** "Import bundle": path + Browse… (`.chairphoto` filter, previews automatically) +
Check.
Preview: batch label, photos, new, already in catalog, no-op notice.
"Import N new".
Result: added, originals copied, duplicates skipped, tags created, errors.

**Keys:** Path input: Enter checks

**Commands / events:** `pickBundleFile` (dialog), `previewBundle`→preview_bundle,
`importBundle`→import_bundle_cmd

**Port ticket:** Storage and import (bundles are not named in any ticket;
assigned by fit)

**Status:** built (#114: `crates/app/src/storage/bundle_import.rs` `BundleImport`), awaiting
the visual check: path + Browse… (previews at once) + Check, Enter checks, preview (label,
photos, new, already in catalog, no-op notice), "Import N new" as a background import with
its result.
Difference: GPUI's portal picker takes no filter (`PathPromptOptions` has no field;
gpui-pre-linux 0.3.7 sets none), so the pick is previewed like a typed path and the core's
check of its contents decides: a renamed real bundle (`.chairphoto.zip`, no extension) is
accepted, anything else refused with the core's reason, worded as "not a ChairPhoto bundle"
when the name is not `.chairphoto` (#161, review L6: `preview_error`).

### `src/components/BundleExportDialog.tsx`

**Spec:** "Export batch as bundle": batch label, destination + Browse…, filename (default
from the label, `.chairphoto` added), note, "Export bundle" ("Progress shows in the
topbar").
Result: exported, skipped offline, failed.

**Keys:** —

**Commands / events:** `pickFolder` (dialog), `exportBundle`→export_bundle;
progress via `import:progress` in App

**Port ticket:** Albums and export (bundles are not named in any ticket;
assigned by fit)

**Status:** built (#115: `crates/app/src/export/bundle.rs`, opened from Import ▾ → Export a
bundle, the batch row's ⬇ and the Export dialog), awaiting the visual check: batch label,
destination + Browse…, filename defaulting to the label with `.chairphoto` added, "Export
bundle" as a bench-owned job with Cancel, result (exported, skipped offline, failed).

## Albums and export

### `src/components/AlbumsPanel.tsx`

**Spec:** Sidebar "Albums": ＋ New album (prompt).
Row: name (click toggles the filter), "+N" adds the current selection, count, ⚙ Rename
(prompt), ✕ Delete (confirm;
clears the filter if active).
"No albums yet".

**Keys:** —

**Commands / events:** `listAlbums`→list_albums, `createAlbum`→create_album,
`renameAlbum`→rename_album, `deleteAlbum`→delete_album;
`window.prompt`/`confirm`

**Port ticket:** Albums and export

**Status:** built (#115: `crates/app/src/albums/panel.rs`, prompt `albums/prompt.rs`),
visually checked 2026-10-03 (#163): ＋ New album (inline name prompt), name toggles the filter, "+N"
adds the selection, count, ⚙ Rename, ✕ Delete behind a confirm (clears an active filter),
"No albums yet".
Seen 2026-10-03: ＋ New album prompt, the new album's count, +N adding the selection, the
filter chip; rename/delete not exercised; the +N sat off the row (#180), inline since
1c44374.

### `src/components/SmartAlbumsPanel.tsx`

**Spec:** Sidebar "Smart albums": ＋ opens the editor.
Row: name (toggles the filter), count, ⚙ Edit rule, ✎ Rename (prompt), ✕ Delete (confirm).
Reloads and selects the album after a save.

**Keys:** —

**Commands / events:** `listSmartAlbums`→list_smart_albums,
`renameSmartAlbum`→rename_smart_album, `deleteSmartAlbum`→delete_smart_album

**Port ticket:** Albums and export

**Status:** built (#115: `crates/app/src/albums/panel.rs`), awaiting the visual check: ＋
opens the editor, name toggles the filter, count, ⚙ Edit rule, ✎ Rename, ✕ Delete behind a
confirm, reload and select after a save.

### `src/components/SmartAlbumEditor.tsx`

**Spec:** New/Edit smart album: name;
AND-only condition rows.
Fields: camera make/model, lens, ISO, aperture, focal length, shutter;
rating, colour label, pick state;
capture date;
tag, import batch, flags has-gps/monochrome/is-raw.
Operators depend on the field (is, ≥, ≤, between, is not, is set, contains, before, after,
under incl. children).
Typed value widgets (number, text, date, range, tag picker with counts, batch picker, enum).
✕ / "＋ Add condition".
Live match count (300 ms debounce).
Save/Create.

**Keys:** —

**Commands / events:** `listTags`→list_tags, `listImportBatches`→list_import_batches,
`smartAlbumCount`→smart_album_count, `createSmartAlbum`→create_smart_album,
`setSmartAlbumRule`→set_smart_album_rule, `renameSmartAlbum`→rename_smart_album

**Port ticket:** Albums and export

**Status:** built (#115: `crates/app/src/albums/smart_editor.rs`, fields/operators
`albums/rule.rs`), visually checked 2026-10-03 (#163): name, AND-only rows over every listed field
(grouped menu), field-specific operators, typed values (number, text, YYYY-MM-DD date,
between pair, tag picker with counts, batch picker, enum), ✕ / "＋ Add condition", live count
300 ms debounce, Save rule / Create then selects the album.
Seen 2026-10-03: name, grouped field menu, Rating condition with a typed value and the live
count (8 → 2); not saved.

### `src/components/ExportPanel.tsx`

**Spec:** "Export N photo(s)": preset Hand-off (RAW + XMP) / Show off (JPEG) with a hint
naming the rendered version;
destination (default `~/Pictures/Export`, no picker, not persisted);
"Reach hashtags": tag group + limit, live preview, **Copy** (clipboard), `hashtags.txt`
written with the export.
"Export as bundle…" when a batch is the scope.
Result: exported, skipped offline, failed.

**Keys:** —

**Commands / events:** `listTagGroups`→list_tag_groups,
`assembleHashtagBundle`→assemble_hashtag_bundle, `exportPhotos`→export_photos;
`navigator.clipboard.writeText`

**Port ticket:** Albums and export

**Status:** built (#115: `crates/app/src/export/panel.rs`), visually checked 2026-10-03 (#163):
presets Hand-off / Show off with the version hint, destination default `~/Pictures/Export`
(typed, not persisted), Reach hashtags (group, limit, live preview, Copy to the OS
clipboard, `hashtags.txt`), "Export as bundle…" when a batch is the scope, result;
adds Cancel.
Seen 2026-10-03: presets, destination, Reach hashtags; no export run.

## Bundled modules

The 15 modules in `src/modules/bundled.ts`, in registration order (which fixes the order of
the toolbar action groups and makes the first-registered edit renderer win):

1. `aiTaggingModule` (`ai`), `src/modules/plugins/aiTagging.tsx`
2. `basicEditorModule` (`basic-editor`), `src/modules/plugins/basicEditor.tsx`
3. `tagGraphModule` (`tag-graph`), `src/modules/plugins/tagGraph.tsx`
4. `statisticsModule` (`statistics`), `src/modules/plugins/statistics.tsx`
5. `instagramModule` (`instagram`), `src/modules/plugins/instagram.tsx`
6. `flickrModule` (`flickr`), `src/modules/plugins/flickr.tsx`
7. `smugmugModule` (`smugmug`), `src/modules/plugins/smugmug.tsx`
8. `collageModule` (`collage`), `src/modules/plugins/collage.tsx`
9. `slideshowModule` (`slideshow`), `src/modules/plugins/slideshow.tsx`
10. `localsendModule` (`localsend`), `src/modules/plugins/localsend.tsx`
11. `snapchatModule` (`snapchat`), `src/modules/plugins/snapchat.tsx`
12. `obsidianModule` (`obsidian`), `src/modules/plugins/obsidian.tsx`
13. `mapModule` (`map`), `src/modules/plugins/map.tsx`
14. `facesModule` (`faces`), `src/modules/plugins/faces.tsx`
15. `smartTaggingModule` (`smarttags`), `src/modules/plugins/smartTagging.tsx`

Every module starts disabled until the user enables it in Preferences → Modules. All but
`basic-editor` declare a `permissions.commands` list. The gate itself is being dropped, but
the list is a ready inventory of each module's backend surface. The first-party modules do
not use `api.fetch` / `module_fetch`: a grep of `src/modules/plugins/` for
`\bfetch\b|moduleFetch|module_fetch|api\.fetch` finds only comments and "re-fetch".

### `src/modules/plugins/aiTagging.tsx`

**Spec:** `ai`, backend `ai`.
**Inspector panel "AI tags":** engine and model pickers (Ollama models live;
curated cloud lists;
saved custom model kept);
Suggest tags;
▭ Region mode (drag a box on the preview;
taps < 2 % discarded);
"Suggest for N selected" (burst-grouped) with a cloud bulk-cost confirm (price table,
representatives, Proceed/Cancel);
suggestion rows (path, "new", confidence, reason, description, synonyms;
✓ add, ✓ all(N), ✗ reject, ↳ more specific);
propagated groups (accept or reject group);
Re-run;
follow-up question + Ask;
toasts.
**Settings:** engine (cloud upload note), per-provider URL/model/API key (password),
existing_only, min_confidence, advanced prompt editor (Load default/Reset), Save.
Keys `ai.provider`, `ai.ollama_url`, `ai.ollama_model`, `ai.cloud_model`,
`ai.cloud_api_key`, `ai.openai_model`, `ai.openai_api_key`, `ai.gemini_model`,
`ai.gemini_api_key`, `ai.existing_only`, `ai.min_confidence`, `ai.prompt_template`.

**Keys:** Follow-up input: Enter asks

**Commands / events:** ai_get_suggestions, ai_suggest_tags, ai_suggest_tags_grouped,
ai_grouped_estimate, ai_accept_suggestion, ai_reject_suggestion, ai_ollama_models,
ai_default_prompt, get_preview;
host selection, settings, toast, notifyChange

**Port ticket:** Per-module ports (ai)

**Status:** built (#126: `crates/app/src/modules/ai_tagging/`), awaiting the visual check:
"AI tags" panel (engine/model pickers, Suggest, ▭ region, "Suggest for N selected" with the
bulk cloud confirm, ✓/✓ all/✗/↳, groups, Re-run, Ask) and settings (per-provider
URL/model/key, existing_only, min_confidence, prompt editor).
Bodies in core `app::ai`.
Difference: a cloud engine also needs its saved API key before any photo is sent.
Visual check 2026-10-03 (#163): settings (engine, Ollama URL/model, Pick…, existing-only,
min confidence, prompt editor) and the inspector panel (engine, model, Suggest tags, Region,
Ask) seen; nothing sent to a provider.

### `src/modules/plugins/basicEditor.tsx`

**Spec:** `basic-editor`, backend `edit`.
Only an **edit renderer** (`render` = loupe fit render of a version's edit;
`renderHi` = zoom / full RAW).
Its presence enables Develop, per-version editing and edited-version loupe renders
(`canEdit`).
No UI.
The Darkroom and presets live in core but keep the `basic-editor.*` keys
(`basic-editor.presets`, `basic-editor.printOnLoupe`).

**Keys:** —

**Commands / events:** `renderForLoupe` → render_edit (engine 1) or `edit://` (engine 2)

**Port ticket:** **No ticket**: not in the per-module list

**Status:** dropped (decision: #104, the Basic Editor folds into the Darkroom;
no edit-renderer contribution).
Its effects are built: the `edit` cargo feature gates `crate::darkroom`
(`crates/app/src/lib.rs:37`) and the rail's Develop (`shell/sidebar.rs:52`) (#111);
edited-version loupe renders through `LoupeView::sync_version` +
`loupe::edit_renders::EditRenders` (#109);
`basic-editor.*` keys kept;
awaiting the visual check.
Difference: Develop is gated at compile time, not by enabling a module.

### `src/modules/plugins/collage.tsx`

**Spec:** `collage`, backend `collage`.
Toolbar action "Make collage" ▦ → CollageDialog.

**Keys:** —

**Commands / events:** (CollageDialog)

**Port ticket:** Per-module ports (collage)

**Status:** built (#125: `crates/app/src/modules/collage/mod.rs`, action "Make collage"),
visually checked 2026-10-03 (#163)
Seen 2026-10-03: "Make collage" in More ⋯ → Modules, and its fewer-than-2 message.

### `src/modules/plugins/CollageDialog.tsx`

**Spec:** "Make collage": snapshots the selection (≥ 2, else a hint);
justified auto-arrange on open.
Template menu (7;
turns Lock on);
Auto-arrange;
Front/Back;
Lock layout.
Canvas (≤ 520×460): drag to move, corner resize (min 6 %), Shift-drag pans inside the frame,
wheel zooms 1–6×;
when locked, drag swaps slots.
Aspect 1:1, 4:5, 5:4, 3:2, 2:3, 16:9, 9:16;
width 1080/2048/4096 with output size;
background colour;
border 0–64;
corner radius 0–128;
JPEG/PNG.
Save to Library (auto-tag `Collage/<kind>`, toast) or Folder (path + Browse…).
Render/Save, Reveal, errors.
No backdrop close.

**Keys:** Shift+drag pans;
wheel zooms a tile

**Commands / events:** collage_auto_arrange, make_collage_freeform, save_collage_to_catalog;
`getThumbnail`→get_thumbnail;
`pickFolder`;
`revealInFolder`

**Port ticket:** Per-module ports (collage)

**Status:** built (#125: `crates/app/src/modules/collage/view.rs`, gestures in
`chairphoto_model::collage`), visually checked 2026-10-03 (#163).
Differences: the template menu is a row of chips, the colour input a hex field with
swatches;
every backend call is bound to the selection's catalog;
a catalog switch closes the dialog.
Seen 2026-10-03: templates, arrange controls, canvas with 8 tiles, aspect, width,
background, border/radius, format, save target; not rendered.

### `src/modules/plugins/collageTemplates.ts`

**Spec:** Layout generators: Grid, Columns, Rows, Feature + column left/right (62/38),
Feature + strip top/bottom.

**Keys:** —

**Commands / events:** —

**Port ticket:** Per-module ports (collage)

**Status:** built (#125: `chairphoto_model::collage`, the 7 templates and canvas gestures,
MIN_TILE 6 %, zoom 1–6×), awaiting the visual check

### `src/modules/plugins/slideshow.tsx`

**Spec:** `slideshow`, backend `slideshow`.
Toolbar action "Make slideshow" ▶ → SlideshowDialog.

**Keys:** —

**Commands / events:** (SlideshowDialog)

**Port ticket:** Per-module ports (slideshow)

**Status:** built (#125: `crates/app/src/modules/slideshow/mod.rs`, action "Make
slideshow"), visually checked 2026-10-03 (#163)
Seen 2026-10-03: "Make slideshow" in More ⋯ → Modules.

### `src/modules/plugins/SlideshowDialog.tsx`

**Spec:** "Make slideshow": snapshots the selection (≥ 2);
thumbnail strip with drag-to-reorder and index badges;
duration 1–15 s;
orientation Landscape/Portrait/Square × 1080p/4K with output size;
Crossfade + duration;
Ken Burns;
FPS 24/30/60;
output folder (default `~/Videos`) + Browse…;
Render with progress ("Preparing frames…" / "Encoding… N%", indeterminate without events);
Reveal;
errors.
Backdrop closes.

**Keys:** —

**Commands / events:** make_slideshow;
ev: `slideshow:progress` (optional);
get_thumbnail;
`pickFolder`;
`revealInFolder`

**Port ticket:** Per-module ports (slideshow)

**Status:** built (#125: `crates/app/src/modules/slideshow/view.rs`, options in
`chairphoto_model::slideshow`), visually checked 2026-10-03 (#163).
Differences: adds Cancel (a newer render or catalog switch also kills ffmpeg);
progress always subscribed, so no indeterminate state;
closing the dialog lets the render finish and reports on the status line.
Seen 2026-10-03: photo order, duration, orientation/resolution, crossfade, Ken Burns, frame
rate, output folder; not rendered.

### `src/modules/plugins/localsend.tsx`

**Spec:** `localsend`, backend `localsend`.
Publish target "Device (LocalSend)" = SendToDevicePanel, records no publication.

**Keys:** —

**Commands / events:** (SendToDevicePanel)

**Port ticket:** Per-module ports (localsend + SendToDevice)

**Status:** built (#123: `crates/app/src/modules/localsend/mod.rs`, publish target "Device
(LocalSend)"), awaiting the visual check
Visual check 2026-10-03 (#163): only the publish target chip "Device (LocalSend)" was seen;
the form was not opened, since showing it starts LAN discovery.

### `src/modules/plugins/SendToDevicePanel.tsx`

**Spec:** Sends the selection (or active photo);
version picker for the active photo;
discovers devices on open;
device dropdown (alias, model, IP) + Refresh;
manual IP + port (53317);
optional PIN;
optional preflight warning;
"Sending d/t…";
Send;
status;
toast "Sent N to X, M skipped";
"Select a photo".

**Keys:** —

**Commands / events:** localsend_discover, localsend_send;
ev: `localsend:progress`;
`listVersions`→list_versions

**Port ticket:** Per-module ports (localsend + SendToDevice)

**Status:** built (#123: `crates/app/src/modules/localsend/send.rs`), awaiting the visual
check.
Differences: adds Cancel (also on a newer send or catalog switch, mid-upload, cancelling the
receiver session);
progress counts only this send's job;
the device scan runs when the form is first shown (the Publish dialog builds a form when its
chip is chosen).

### `src/modules/plugins/snapchat.tsx`

**Spec:** `snapchat`, backend `localsend`, requires `localsend`.
Publish target "Snapchat" = SendToDevicePanel that records a `snapchat` publication, with a
9:16 (±3 %) preflight warning based on the version's crop or the photo's dimensions.

**Keys:** —

**Commands / events:** localsend_discover, localsend_send, `localsend:progress`;
record_publication

**Port ticket:** Per-module ports (snapchat)

**Status:** built (#123: `crates/app/src/modules/localsend/mod.rs` `SnapchatModule`, 9:16 ±3
% preflight in `chairphoto_model::publishing`), awaiting the visual check.
Difference: records a publication only for photos that reached the device (React recorded
every selected one).
Visual check 2026-10-03 (#163): only the publish target chip and "Requires: LocalSend" were
seen; the form was not opened (LAN discovery).

### `src/modules/plugins/obsidian.tsx`

**Spec:** `obsidian`, no backend.
**Inspector "Note":** Create note in Obsidian (frontmatter: type, uuid, file, captured,
camera, lens, exposure, rating, ≤ 20 tags;
links `chairphoto://uuid` and `/loupe`);
when linked: filename, Open note, Forget.
**Tag-editor "Obsidian note":** the same for tags (aliases from terms, description,
`chairphoto://tag/uuid`).
**Settings:** vault, notes folder (default ChairPhoto).
Keys `obsidian.vault`, `obsidian.folder`, `obsidian.note.<uuid>`, `obsidian.tagnote.<uuid>`.

**Keys:** —

**Commands / events:** get_photo, get_photo_tags, `listTagTerms`→list_tag_terms;
`openExternal` (`obsidian://new`, `obsidian://open`)

**Port ticket:** Per-module ports (obsidian)

**Status:** built (#128: `crates/app/src/modules/obsidian/`, notes/URIs/records in
`chairphoto_model::obsidian`), awaiting the visual check: inspector "Note", tag-editor
"Obsidian note", settings;
URIs via `App::open_url`;
writes bound by `CatalogIdentity`;
settings validated.
Visual check 2026-10-03 (#163): settings (vault, notes folder, Save) and the inspector
"Create note in Obsidian" seen; not clicked (no Obsidian launch).

### `src/modules/plugins/publishing.tsx`

**Spec:** Shared, not a module.
**OAuthSettings:** API key, secret, max long edge;
signup hint;
Save keys;
Connect/Reconnect (opens the browser, fallback link);
paste verifier + Finish (OAuth 1.0a OOB);
Connected ✓.
Keys `<id>.api_key`, `<id>.api_secret`, `<id>.max_long_edge`.
**PublishPanel:** version picker, title, description, tags (when the service suggests tags;
prefilled until edited), album (cached `albums_cache`, `last_album`, Refresh, "+ New"),
Publish, records the publication with its URL, toast.

**Keys:** Verifier: Enter finishes.
New album: Enter creates.

**Commands / events:** per-service commands;
`openExternal`;
`listVersions`→list_versions;
record_publication

**Port ticket:** Per-module ports (publishing)

**Status:** built (#123/#124: `crates/app/src/modules/publishing/` — `OAuthSettings`
(oauth.rs) and `PublishPanel` (panel.rs) over `PublishService`;
publish is core's job `app::uploads`), awaiting the visual check.
Differences: steps Preparing…/Rendering…/Uploading… with Cancel until the upload starts;
`OAuthSettings` follows a catalog switch.

### `src/modules/plugins/flickr.tsx`

**Spec:** `flickr`, backend `flickr`.
**Settings:** OAuthSettings + "Import published from Flickr" (Preview counts
matched/ambiguous/not in catalog;
≤ 50 matches with both thumbs;
resolve ambiguous items by clicking a candidate, Undo;
"Import N publications" with historical dates;
never writes to Flickr).
**Publish target** with tags prefilled from keywords.

**Keys:** —

**Commands / events:** flickr_begin_auth, flickr_complete_auth, flickr_connected,
post_to_flickr, flickr_suggest_tags, flickr_import_published, flickr_import_apply;
`thumb://`;
remote Flickr thumbnails

**Port ticket:** Per-module ports (flickr) — #124

**Status:** built (#124: `crates/app/src/modules/flickr/` — OAuth settings, "Import
published from Flickr" with ambiguous resolution/Undo (import.rs), publish target with
keyword tags;
bodies in core `app::{oauth, flickr, uploads}`), awaiting the visual check.
Registers without the opt-in `flickr` feature and is refused with the reason;
the import is bound to the catalog it matched;
Flickr thumbnails only from `*.staticflickr.com` over HTTPS, ≤ 1 MiB, no redirects.
Visual check 2026-10-03 (#163): in the default build it lists as "backend “flickr” not
included in this build" and cannot be enabled; its settings and import were not reachable.

### `src/modules/plugins/smugmug.tsx`

**Spec:** `smugmug`, backend `smugmug`.
**Settings:** OAuthSettings.
**Publish target** with album picker, Refresh and create album (no tags).

**Keys:** (Enter handlers from publishing)

**Commands / events:** smugmug_begin_auth, smugmug_complete_auth, smugmug_connected,
smugmug_list_albums, smugmug_create_album, post_to_smugmug

**Port ticket:** Per-module ports (smugmug) — #124

**Status:** built (#124: `crates/app/src/modules/smugmug/`, bodies in core
`app::{oauth, smugmug, uploads}`), awaiting the visual check.
Registers without the opt-in `smugmug` feature and is refused with the reason;
the publish is a cancellable job.
Visual check 2026-10-03 (#163): in the default build it lists as "backend “smugmug” not
included in this build" and cannot be enabled.

### `src/modules/plugins/instagram.tsx`

**Spec:** `instagram`, backend `instagram`;
login through Chrome.
**Publish target:** version picker, caption (title + #hashtags until edited), "Publish
automatically" (default off: stops before Share), note, Post.
Outcomes: needs login;
awaiting review ("Did you click Share?" → "Yes, I posted it" records / "No, skip");
posted → records + toast.

**Keys:** —

**Commands / events:** post_to_instagram, build_instagram_caption, list_versions;
record_publication

**Port ticket:** Per-module ports (instagram) — #124

**Status:** built (#124: `crates/app/src/modules/instagram/`, body in core `app::instagram`
behind `InstagramDriver`), awaiting the visual check.
Differences: Cancel until Chrome has the render;
the confirmation records the composed photo+version in its own catalog;
Post is refused while a review is open.
Visual check 2026-10-03 (#163): the Publish dialog's Instagram form (version, caption,
publish-automatically, Post to Instagram) seen; nothing posted.

### `src/modules/plugins/faces.tsx`

**Spec:** `faces`, backend `faces`.
**Main view "People":** Refresh;
"Review suggestions (N)";
named-people wall (avatar crop, name, photo/face counts;
click filters by tag);
unnamed clusters (click → name modal with a people-tag type-ahead).
Suggestions queue: ← People, confidence slider (default 0.8), "Confirm all ≥ X% (n)",
per-row ✓/✕.
**Inspector "Faces":** per face #, name, confidence, state;
✓ confirm, "✓✓ confirm on N" (whole selection, toast), ✕ reject, ⇄ reassign, – ignore, 🗑
delete drawn.
**Loupe overlay:** boxes coloured by state that follow zoom/pan;
name chip with ✓ ✕ ⇄ – 🗑;
PersonPicker (search, "＋ Create" person);
"＋ face" draw mode (one drag = one box, min 8 px);
hide/show faces (localStorage `faces.showBoxes`);
also used by the pop-out loupe.
**Settings:** model status (YuNet + AuraFace) with Download/Re-download;
inference line (CUDA / CPU fallback / idle);
indexing speed Background/Full (next start);
people root with type-ahead;
match threshold (default 0.45);
Save;
"Index faces" / "Run matching" with Cancel, progress, last result, error;
re-attaches to a running job.
Keys `faces.people_root`, `faces.match_threshold`.
Suggested split: A = per-photo review (overlay, picker, inspector);
B = jobs and people (settings, job state, People view, clusters, suggestions).

**Keys:** PersonPicker: ↑/↓, Enter picks or creates, Esc cancels.
Loupe overlay: **F** toggles boxes (no modifiers, not in inputs);
Esc leaves draw mode.
People-root: ↑/↓, Enter, Esc.
Name-cluster modal: Enter saves, Esc cancels.

**Commands / events:** faces_for_photo, faces_accept, faces_accept_person, faces_reject,
faces_ignore, faces_assign, faces_add_manual, faces_delete_drawn, faces_models_status,
faces_download_models, faces_inference_info, faces_set_indexing_speed, faces_index_photos,
faces_index_status, faces_index_cancel, faces_run_matching, faces_match_status,
faces_match_cancel, faces_people_summary, faces_cluster_summary, faces_suggestion_list,
faces_name_cluster;
`createTag`→create_tag;
`thumb://`;
ev: `faces:progress`, `faces:match_progress` (cosmetic), `faces:index_done`,
`faces:match_done` (required;
fail closed), job-filtered

**Port ticket:** Per-module ports (faces, likely two tickets)

**Status:** built (#129: `modules::faces` settings.rs, inspector.rs, overlay.rs, picker.rs;
#130: people.rs, people_view.rs), awaiting the visual check: settings with Index faces / Run
matching jobs (adopts a run started elsewhere), inspector Faces block (✓, ✓✓ confirm on N,
✕, ⇄, –, 🗑), loupe overlay with PersonPicker, ＋ face draw mode and F, People view as tabs
(wall → filter, Unnamed clusters with naming, merge/split, Review suggestions with threshold
and Confirm all ≥ X%).
Differences: `faces.showBoxes` in machine prefs;
boxes hidden over an edited version's render;
avatars uniformly scaled (React stretched);
People writes wait while a match runs.
Visual check 2026-10-03 (#163): settings (models ready, inference, speed, people root,
threshold), the inspector block's empty state and the People view (tabs, empty state,
Refresh) seen; indexing, matching and the loupe overlay not run (no faces indexed). The rail
button has no icon (#173).

### `src/modules/plugins/map.tsx`

**Spec:** `map`, backend `map`.
**Main view "Map":** Leaflet over raster tiles, auto-fit to GPS points (max zoom 12);
clustered markers (radius 60, spiderfy at max zoom);
a marker/cluster opens a filmstrip ("N photos at this location") and silently selects the
first photo so the pop-out loupe follows;
"Show in Library";
×.
Loading, error, "No photos with GPS data".
**Fences panel:** "+ Draw"/Cancel;
click adds vertices, double-click or clicking near the first vertex closes;
fence editor (name, tag path);
coloured polygons with draggable vertices (saves on drag);
Apply (toast), Edit, Delete (confirm);
"Apply all fences".
**Settings:** tile URL `map.tileUrl` (default OSM) Save/Reset, attribution;
"Geocode all with GPS" (Nominatim ≤ 1 req/s, fills only empty IPTC City/State/Country/Code)
with progress.
**Inspector "Geocode":** per-photo Geocode location.
Network: tiles and Nominatim are reached only while the (default-off) module is enabled;
the file has no extra opt-in.

**Keys:** Filmstrip: window keydown Esc closes.
Fence dialog: Enter saves, Esc cancels.

**Commands / events:** map_photo_points, list_fences, create_fence, update_fence,
delete_fence, apply_fence, apply_all_fences, geocode_all_to_iptc, geocode_to_iptc;
ev: `geocode:progress` (optional);
`thumb://`

**Port ticket:** Slippy map (design) then Per-module ports (map module)

**Status:** built (#119: `crates/app/src/modules/map/` — view.rs canvas with fit to points
(pad 0.05, max zoom 12), core grid clustering (60 px), filmstrip "N photos at this location"
with quiet select, Show in Library, ×/Esc (#162: a horizontal virtual list over every photo
at the marker, thumbnails for the frames on screen ± 8, nearest the active photo first,
under its own image claim;
closes on a catalog switch);
fences draw/close, editor (Enter saves, Esc cancels), vertex drag saves, Apply/Edit/Delete
(confirm), Apply all fences;
settings.rs tile URL Save/Reset, attribution, Geocode all with progress and Cancel;
inspector "Geocode" panel;
tiles.rs disk-cached tiles), awaiting the visual check.
Changes: tiles ask per host, remembered per machine (`map.tileHosts`, decision #118), plain
graticule until allowed;
toasts are status lines.
Not ported: spiderfy (dead in React: `clusterclick` was switched off).
Visual check 2026-10-03 (#163), tiles blocked in the agent's prefs: cluster marker, Fences
card, + / −, "N photos with GPS", "Map tiles off", the marker's filmstrip ("4 photos at this
location", quiet select, Show in Library, ×) and the settings (tile URL, tile servers,
reverse geocoding) seen; no tiles loaded, no geocoding run.

### `src/modules/plugins/smartTagging.tsx`

**Spec:** `smarttags`, backend `smarttags`.
**Inspector "Similar tags":** "Download model (~350 MB)" with progress when missing;
broken custom path error;
Index (N %) with Cancel, re-attaches on mount;
result and error lines;
Suggest;
suggestions (path, confidence, "from N similar photos", ✓ add with toast, ✗ reject).
**Settings:** model status;
Download;
model path `smarttags.model_path` (blank = default);
privacy note;
Save;
"Train classifiers" (status);
"Delete index" (no confirm).

**Keys:** —

**Commands / events:** smarttags_model_status, smarttags_download_model,
smarttags_index_photos, smarttags_index_status, smarttags_index_cancel,
smarttags_suggest_tags, smarttags_load_suggestions, smarttags_accept_suggestion,
smarttags_reject_suggestion, smarttags_delete_index, smarttags_train_classifiers;
ev: `smarttags:progress`, `smarttags:download_progress` (cosmetic), `smarttags:index_done`
(required;
fails closed)

**Port ticket:** Per-module ports (smartTagging)

**Status:** built (#126: `crates/app/src/modules/smart_tagging/`, bodies in core
`app::smarttags`), awaiting the visual check: "Similar tags" panel (Download model (~350
MB), Index/Cancel with re-attach, Suggest, ✓/✗) and settings (model path, Train classifiers,
Delete index);
`smarttags:index_done` ends a run.
Visual check 2026-10-03 (#163): settings (model ready, path, Save, Train classifiers, Delete
index) and the inspector's Index/Suggest seen; no index run.

### `src/modules/plugins/statistics.tsx`

**Spec:** `statistics`, always available.
**Main view "Stats"**, scoped to the sidebar tag/album/batch (scope chip + hint).
Skeleton, error.
Stat cards (Photos, Date range, Cameras, Lenses);
facts (busiest day/year, favourite hour, weekend vs weekday);
timeline area chart per month (per year beyond 180 months) with hover readout and
invalid-date footnote;
24 h radial clock;
day-of-week bars;
camera donut (top 5 + Other) and rank list;
top tags (click filters);
lenses;
bars for focal length, rating, ISO, aperture, shutter;
cull survival;
≥ 4★ hit rate;
"Keeper analysis" (keep rate / ≥ 4★) by lens, camera, focal, ISO, aperture, shutter.
Tooltips.
Per-scope cache (16), cleared on catalog switch.

**Keys:** —

**Commands / events:** catalog_stats;
ev: `catalog:switched` (optional)

**Port ticket:** Per-module ports (statistics)

**Status:** built (#127: `crates/app/src/modules/statistics/` view.rs over
`chairphoto_model::statistics`), awaiting the visual check: scope chip, skeleton, error, 4
cards, facts, timeline, 24 h clock, weekday bars, camera donut + list, top tags (click
filters), lenses, focal/rating/ISO/aperture/shutter bars, cull survival, ≥ 4★ hit rate,
keeper analysis, 16-scope cache cleared on switch.
Differences: the timeline readout is the chart tooltip;
grow-in animations and star-label tinting not ported.
Visual check 2026-10-03 (#163): scope chip, cards, facts, timeline, clock, weekday bars,
camera donut and list, top tags, lenses, focal bars seen; a one-month scope draws an empty
timeline (#179); the rail button has no icon (#173). Since 1c44374 a one-month scope draws
a level line across the chart with its tick in the middle; React fills a lopsided triangle
there (an artifact of its area path), GPUI fills the whole band under the line.

### `src/modules/plugins/tagGraph.tsx`

**Spec:** `tag-graph`, always available.
**Main view "Graph"** on one canvas.
**Communities mode:** radial edge-bundled ring (β 0.85), community arcs and labels, camera
arc, bundled co-occurrence/camera edges, collision-free labels, hover/selection lights
neighbours.
**Photo ↔ tag mode:** d3-force bipartite graph capped at 1500 photos;
Freeze layout;
Thumbnails.
**Left panel:** branch breadcrumbs;
node-type toggles (all off at first, with a hint);
communities list (click focuses);
link-strength slider (0–20).
**Canvas:** wheel zoom (0.05–6), drag pans and deselects, click selects, drag moves nodes in
force mode, hover tooltip;
legend;
−/＋/Fit/Re-center;
status strip.
**Right inspector:** chips, stats, connected (top 8), top 6 photos;
Focus on branch, Filter library, Isolate neighbours / Show all, Open loupe window.
Community card.
Mirrored to the pop-out loupe via `showInLoupe`.

**Keys:** Window keydown Esc: deselect;
with nothing selected, climb one branch level

**Commands / events:** library_graph, photo_tag_graph, list_photos;
`thumb://` (raw `convertFileSrc`)

**Port ticket:** [Port the Tag graph module][t121] (Communities);
Open loupe window and the `showInLoupe` mirror: [Pop-out loupe window][t110]

**Status:** built (#121: `crates/app/src/modules/tag_graph/` view.rs/paint.rs/raster.rs over
`chairphoto_model::tag_graph`;
Open loupe window and the `show_in_loupe` mirror #110), visually checked 2026-10-03 (#163):
Communities ring, breadcrumbs, toggles, communities list, link strength,
zoom/pan/select/hover, legend, −/＋/Fit/Re-center, inspector actions (Focus on branch, Filter
library, Isolate/Show all, top photos), Esc.
Labels horizontal, edges soft while zooming ([#120][t120]). dropped (decision: [#120][t120],
Communities only): Photo ↔ tag mode with Freeze layout, Thumbnails, node drag,
`photo_tag_graph`.
Seen 2026-10-03: node-type toggles, communities list, link strength, Communities ring,
legend, −/＋/Fit/Re-center, a selected tag's card (chips, stats, connected, top photos) and
its actions; Open loupe window not clicked (pop-out, needs the owner).

### `src/modules/plugins/tagGraphBundle.ts`

**Spec:** Ring layout for community mode: trie over tag paths (cameras under `__camera`),
depth-first ring order, sibling/group gaps, LCA control polylines, `bundlePath` (d3
curveBundle + curveBasis), `relativeToBranch`, `parentPath`.

**Keys:** —

**Commands / events:** —

**Port ticket:** [Port the Tag graph module][t121] (`chairphoto_model::tag_graph::bundle`,
its 17 tests one to one)

**Status:** built (#121: `chairphoto_model::tag_graph::bundle`, its 17 tests one to one),
awaiting the visual check

## Module infrastructure and core API

### `src/modules/api.ts`

**Spec:** Typed wrappers for **core** commands: 164 distinct commands, 192 exported values.
**List cache:** list_tags, list_facets, distinct_photo_values, list_tag_groups,
recently_used_tags are cached by command and args, with in-flight requests shared.
Any command not on the neutral allowlist counts as a mutation and bumps the cache
generation.
**Thumbnail limiter** (6 at a time).
**Media URLs:** `thumbnailUrl` (`thumb://`), `editRenderUrl` (`edit://…`), `renderForLoupe`
(engine 2 → `edit://` at 2560 px, else render_edit), `assetUrl`, `videoServerPort`.
**Deep links:** `chairphoto://<uuid>[/loupe|/develop]`, `chairphoto://tag/<uuid>`.
**Pickers:** folder, file, `.chairphoto` bundle.
**Opener:** openExternal, revealInFolder, revealPhoto. appVersion.

**Keys:** —

**Commands / events:** 164 commands;
ev (10): `catalog:switched`, `appearance:theme_changed`, `cache:progress`,
`import:progress`, `scan:progress`, `develop:progress`, `rapidraw:progress`,
`develop:source`, `identity:repair_progress`, `identity:repair_done`;
deep-link `onOpenUrl` ×2

**Port ticket:** Deep links (URL parse);
Image layer (media URLs, limiter);
Shell APIs (pickers, confirm);
**list cache: no ticket**

**Status:** dropped (decision: #104/#99, views call core services directly): the IPC wrapper
layer, media URLs (replaced by the Image layer `crates/app/src/image_store.rs`, #101, which
also bounds concurrency), `appVersion` (only for third-party `minHostVersion`).
The list cache is dropped by design: each list lives in one entity that is its own cache,
refetched on events and by the mutating code (`crates/app/src/model.rs` module doc, #99).
Built, awaiting the visual check: deep links (#100: `chairphoto_model::deep_link::parse`,
`AppModel::open_deep_link`, `single_instance.rs`);
folder/file pickers via `prompt_for_paths` (`modules::dialog::pick_folder` and per-dialog
Browse…);
opener via `App::open_url` / `reveal_path` / `open_with_system`.
Difference: file pickers have no extension filter (`.chairphoto`, `.cube`).

### `src/modules/host.ts`

**Spec:** **Survives as the Module trait:** registry entry (id, enabled, panels, actions,
settings, main views, publish targets, edit renderer);
panel slots `inspector|sidebar|loupe|tag-editor`;
toolbar actions grouped by module;
first-enabled edit renderer wins;
hooks onLoad/onUnload/onPhotoSelected/onActivate, run safely (a failing onLoad rolls back
with a toast);
`requires` (enable dependencies first, cascade-disable dependents, `modules.enabled` in
dependency order);
`backendFeature` via plugin_features;
namespaced settings `<id>.<key>`;
publication marker;
host → module state (selection, active photo/version, editing tag, filter context);
toast/change/nav sinks;
owned loupe card;
onEvent;
ModuleInfo for Preferences.
**Third-party only:** external flag, `minHostVersion`, semver, `loadExternalModules`, stubs,
shape validation, permission grants (`modules.permissions`), origin allowlists,
`refuseInvoke`/`refuseFetch`, `api.fetch`.

**Keys:** —

**Commands / events:** appVersion, assetUrl, assignTag, deletePublication, getEditRecord,
getSetting, listExternalModules, listPublications, listTags, moduleFetch, pluginFeatures,
recordPublication, setEditRecord, setSetting;
raw invoke/listen for modules;
loupe broadcastCard/onLoupeReady/openLoupeWindow

**Port ticket:** Module trait

**Status:** built (#104/#122: `crates/app/src/modules/mod.rs`
`Module`/`ModuleInstance`/`ModuleHost`, `registry.rs` `ModuleRegistry`), awaiting the visual
check: slots inspector/sidebar/loupe/tag-editor, actions grouped by module, `load` Err rolls
back with a status line, `requires` with cascade and `modules.enabled` in dependency order,
`backend_feature`, namespaced `ModuleSettings`, publication marker, `on_event`, owned loupe
card (`show_in_loupe`), `ModuleInfo`.
Toasts are status lines. dropped (decision: #104): external loader, semver, permission
grants, origin allowlists, `module_fetch`, `onPhotoSelected`, `getEditRecord`, the
edit-renderer contribution.

### `src/modules/registry.ts`

**Spec:** Contract types.
**Survive:** Photo/Tag/Publication DTOs, EditRecord, EditRenderer, ModulePanel, MainView,
ToolbarAction (fire-and-forget or modal with close), PublishTarget, SettingsPanel,
LoupeCard/scope, the ChairPhotoAPI surface, ChairPhotoModule (id, name, version,
description, backendFeature, publicationMarker, requires, hooks).
**Third-party only:** ModulePermissions, fetch types, the DOM `mount` ABI, semver ranges on
`requires`.

**Keys:** —

**Commands / events:** —

**Port ticket:** Module trait

**Status:** built (#104/#122: `ModuleMeta`, `Contributions`, `Panel`, `MainView`,
`ModuleAction` (`ActionKind::Run`/`Modal`), `PublishTarget`, `SettingsPanel` in
`crates/app/src/modules/mod.rs`;
DTOs are core types), awaiting the visual check. dropped (decision: #104): permissions,
fetch types, DOM mount ABI, semver ranges.

### `src/modules/ownedEvents.ts`

**Spec:** Listener ownership: owner-tagged slots;
a registration that resolves late is stopped;
release only by the owner;
`useOwnedSubscription`;
`forJob` drops events from other jobs;
`terminalBuffer` buffers terminal events until the job id is adopted.

**Keys:** —

**Commands / events:** wraps any subscribe

**Port ticket:** App crate (event bridge: subscriptions drop with the entity;
job filtering stays)

**Status:** built (#99: `crates/app/src/events.rs` `GpuiSink` → `route` to owning entities;
subscriptions drop with their entity;
job events filtered by job id in the owning state, e.g.
`modules/faces/state.rs`, `modules/smart_tagging/state.rs`, `storage/identity_debt.rs`).
No UI to check.

### `src/modules/ModuleContent.tsx`

**Spec:** Adapter between React `render` and the DOM `mount/unmount` of external modules;
ModuleSettings;
ModuleActionModal (binds `close`).

**Keys:** —

**Commands / events:** —

**Port ticket:** Module trait

**Status:** dropped (decision: #104, React/DOM adapter for external modules).
The surviving part, "a modal action gets a close callback", is built as `ActionKind::Modal`
closed with `window.close_dialog` (`crates/app/src/modules/mod.rs:221`, #122).

### `src/modules/bundled.ts`

**Spec:** The static list of the 15 bundled modules (above), each disabled until enabled.

**Keys:** —

**Commands / events:** —

**Port ticket:** Module trait (compiled-in registry)

**Status:** built (#122 + per-module ports: `crates/app/src/modules/mod.rs` `bundled()`,
each behind its cargo feature;
every module starts disabled), awaiting the visual check.
Registration follows `BUNDLED_MODULES` (`basic-editor` is gone): `tag_graph` moved from
after `faces` to right after `ai` (#161, test `bundled_modules_register_in_reacts_order`).

## Components that fit no existing ticket

These rows have no ticket that names them. Each has a proposed ticket and the question it
would answer.

| Row | Proposed ticket | Question |
|---|---|---|
| StackProposalsDialog | **Library whole-view tools: stack proposals** | Should the "Stack bursts" dialog (and the burst-analysis and cull entry points it shares with the Bench and ⋯ menu) join the [Library view][t106] ticket, or get its own port with the keeper-override and per-group apply flow? |
| TrashDialog | **Trash dialog** | Is the Trash dialog (restore, typed-`delete` permanent delete, unreachable-disk report) part of [Storage and import][t114], or its own ticket beside the grid context menu's Move to trash? |
| basicEditor | **Basic editor into the core Darkroom** | The map's per-module list omits `basic-editor`. It has no UI; it only registers the edit renderer whose presence enables Develop. Does it fold into the core Darkroom behind the `edit` feature (keeping the `basic-editor.*` setting keys), or stay a Rust module that provides an edit renderer? |
| api.ts list cache | **List cache and invalidation in the GPUI app** | The five cached list reads and the "unknown command = mutation" invalidation rule are not in the [Library logic][t102] list. Where does this cache live once views call core services directly, and does it survive at all? |
| BundleImportDialog, BundleExportDialog | (no new ticket; confirm the fit) | No ticket names `.chairphoto` bundles. This checklist assigns import to [Storage and import][t114] and export to [Albums and export][t115]; those tickets should confirm the fit. |

## Findings to carry into the ports

- **Keys are scattered and some collide.** App's global handler ignores modifiers (Ctrl+P
  also picks), and its input guard covers only INPUT/TEXTAREA. In the Darkroom, EditStage
  (Enter/Esc), Filmstrip (←/→) and DarkroomView (Ctrl+S/Z/Y) all listen on the window in
  bubble phase, so EditStage's Esc also fires while the proof sheet or duel is open. Only
  ProofSheet, DuelView and GlSpike use the capture phase. The keymap in the [App crate][t99]
  should make these precedences explicit (see [Shell APIs][t96]).
- **The Compare keys live in App, not in CompareView.**
- **Confirmation styles are mixed:** Tauri `confirm` (Preferences, remove from catalog),
  `window.confirm`/`prompt` (volumes, albums, smart albums, map fences), inline confirms
  (trash, identity debt, tag delete), and none at all (tag-group delete, version delete,
  publication delete, preset delete, smart-tags "Delete index"). The plan moves all of
  these to one dialog component.
- **Settings outside the catalog:** `appearance.mode`, `panel.*`, `inspector.section.*` and
  `faces.showBoxes` are in localStorage. The GPUI app needs a per-machine store for them.
- **Dead code seen during the inventory:** EditStage's `showBefore`/`topLeft` (no
  Before/After in the Darkroom); TitleBar's `children` slot; PreviewImage's stale "used by
  the loupe" comment. These are not porting targets.
- **Map privacy:** the only gate on tiles and Nominatim is enabling the (default-off) Map
  module. The plan's "same opt-in as today" means exactly that; a stricter gate would be
  new behaviour.
- **Pop-out loupe appearance:** a mode change does not reach an open loupe window today.
  Shared entities fix this for free.

## Completeness check

Commands run at `ebe3259` from the repository root:

```bash
find src -name '*.tsx' -not -path '*__tests__*' | sort            # 79 files
find src -name '*.ts' -not -path '*__tests__*' \
  -not -path '*__test_stubs__*' -not -name '*.test.ts' | sort     # 35 files
```

That is 114 files. There are no `*.test.ts(x)` files outside `__tests__`. The seven
`src/__test_stubs__/*.ts` files (Tauri API stubs for vitest) are excluded as test stubs.

Rows per area:

| Area | Rows |
|---|---|
| Shell | 23 |
| Library | 7 |
| Inspector and tags | 14 |
| Loupe and cull | 8 |
| Darkroom | 21 |
| Preferences | 3 |
| Storage and import | 7 |
| Albums and export | 4 |
| Bundled modules | 21 |
| Module infrastructure and core API | 6 |
| **Total** | **114** |

Cross-check: extract every backticked `src/…` path in the first column of the area tables,
and diff it against the `find` output:

```bash
grep -oE '^\| `src/[^`]+`' docs/plans/gpui/parity.md | sed 's/^| `//; s/`$//' | sort > /tmp/rows
uniq -d /tmp/rows                     # no path has two rows
find src \( -name '*.tsx' -o -name '*.ts' \) -not -path '*__tests__*' \
  -not -path '*__test_stubs__*' -not -name '*.test.ts' | sort > /tmp/files
diff /tmp/rows /tmp/files && echo "every file has exactly one row"
```

Result at `ebe3259`: 114 rows and 114 files, no duplicate paths, empty diff ("every file has
exactly one row"). The 15 entries of the bundled-modules list match the order of
`BUNDLED_MODULES` in `src/modules/bundled.ts`. The count of 164 core commands in `api.ts`
includes 5 calls whose command name sits on a continuation line (`get_photo`,
`get_photo_by_uuid`, `list_stack_children`, `module_fetch`, `render_edit_batch`).

## Audit 2026-10-03

Ticket [Parity audit][t157]. Every row was checked against `crates/app/src`,
`crates/model/src` and the core `app::` bodies at `feature/gpui` `5a1dc27`, using the row's
own feature list as the spec. Each status above names the ticket and path that build it.
Absences were confirmed by grep in the worktree, and the searches are named in the cells.
Docs only; nothing was built or run.

No row has been checked in the running app yet. The resolution comments on #99, #105–#115,
#119, #121–#130 and #134 record the visual check as not run (or say nothing of one), so no
row is `ported`.

### Counts

| Status | Rows |
|---|---|
| built, awaiting the visual check | 97 (11 of them logic-only; 6 also carry a dropped part) |
| built and visually checked | 0 |
| partial | 9 (Preferences also carries a dropped part) |
| dropped | 7 |
| to port | 1 |
| **Total** | **114** |

Since the audit (statuses above are current; these counts are the audit's): #159 moved
`CollectionBrowser.tsx`, `shellTiming.ts` and `PhotoInspector.tsx` from partial to built, and
#160 moved `Splash.tsx` from to port to built and closed App.tsx's boot and rescan gaps;
with #158's context menu, nothing in App.tsx's row is missing any more.

Per area (built / partial / dropped / to port): Shell 16/3/3/1; Library 5/2/0/0;
Inspector and tags 12/2/0/0; Loupe and cull 7/1/0/0; Darkroom 20/0/1/0; Preferences 2/1/0/0;
Storage and import 7/0/0/0; Albums and export 4/0/0/0; Bundled modules 20/0/1/0;
Module infrastructure and core API 4/0/2/0.

The dropped rows are `shell/index.ts`, `theme/apply.ts`, `vite-env.d.ts`, `GlSpike.tsx`,
`basicEditor.tsx` (folds into the Darkroom, #104), `api.ts` (views call core directly; the
list cache goes with it, since each entity caches its own reads, `crates/app/src/model.rs`)
and `ModuleContent.tsx`.

The table "Components that fit no existing ticket" above is settled:
- StackProposalsDialog was built in #106.
- TrashDialog was built in #114.
- basicEditor folds into the Darkroom (#104).
- The `api.ts` list cache is dropped.
- Bundles were built in #114 (import) and #115 (export).

### Partial and to-port rows

- **`src/App.tsx`** is missing:
  - The grid context menu: Move to trash, Reveal in Files, Relocate…, Retrieve from NAS and
    Remove from catalog. Right-click only selects (`library/grid.rs`
    `on_tile_right_click`), and nothing in `crates/app` calls `Catalog::trash_photos`, so
    no photo can be trashed from the GPUI app.
  - The loupe's unavailable-state actions. They are still `not_yet_ported!` stubs in
    `shell/actions.rs` that cite #114, which is closed.
- **`src/components/CatalogGrid.tsx`**: the context menu, as under App.tsx.
- **`src/components/Thumbnail.tsx`**: a tile does not follow a cover-look change
  ([#151][t151], open).
- **`src/components/TagMergeModal.tsx`** and **`src/components/Preferences.tsx`**:
  Preferences → Tags → "Merge X away…" still answers "not yet ported (#107)"
  (`preferences/tags.rs` `merge`), although `tags/merge.rs` `TagMerge` is built and opens
  from the Tag panel.
- **`src/components/ZoomableImage.tsx`**: the unavailable-state actions, as under App.tsx.

**Since the audit.** [#158][t158] built the grid context menu and the loupe's
unavailable-state actions: `CatalogGrid.tsx` and `ZoomableImage.tsx` are now built, awaiting
the visual check (the counts above are the audit's), and App.tsx no longer misses those two
items. The `not_yet_ported!` list is empty. A stub may cite only a ticket named in a row's
"Missing:" here (`shell::actions::stub_tickets`, checked by its tests).

### Behaviour gaps noticed

These are outside the rows' missing features, read from code and not run:

- **Stale editor list.** The inspector's "Edit in" list is read once per catalog.
  `Inspector::read_editors` returns early once cached, and only `catalog:switched` clears it.
  A path changed in Preferences → Editors is therefore not seen until a switch.
- **Map filmstrip capped at 200** (fixed by #162). The filmstrip requested thumbnails for
  the first 200 photos only (`modules/map/view.rs` `render_filmstrip`, `take(200)`). It is
  now a horizontal virtual list that asks for the frames on screen as it scrolls.
- **No extension filters on file pickers.** The bundle picker and the LUT picker take any
  file (`prompt_for_paths` has no filter), where React filtered `.chairphoto` and `.cube`.
  A wrong bundle is refused with a message.
- **Identity-debt table not virtualised** (fixed by #162). It rendered all of a 500-row page
  in a scrolling div (`storage/identity_debt.rs`, `debt-rows`); React's table was
  virtualised. Both its lists are virtual lists now.
- **Module registration order differs.** `bundled()` registers the Tag graph after Faces
  (`modules/mod.rs`); React had it third. Only the Modules panel order changes.
- **Stale guard test** (fixed by #158). The `not_yet_ported!` test only checked that the
  cited ticket lay in #93–#130 (`shell/actions.rs`), so stubs citing a closed ticket passed.
- **Cosmetic differences:**
  - The grid's video tooltip says "double-click to play"; a double-click opens the loupe.
  - RenderedImage shows the error text where React showed "—".
  - Toasts are status lines throughout.

### Since the audit

[Preferences and inspector parity polish][t161] (#161), read from code and tests, not run:

- Preferences → Tags → "Merge X away…" opens the Tag panel's merge preview over Preferences.
  `TagMergeModal.tsx` and `Preferences.tsx` move from partial to built (Preferences keeps its
  dropped GlSpike part).
- The stale editor list is fixed: a save in Preferences → Editors (a path, RapidRAW's binary or
  format) emits `model::EditorsChanged`, and the inspector re-reads its "Edit in" list; only
  the newest read lands. The notice is sent from the save's completion whether or not the
  Editors section still exists (saves fire on blur, so a tab switch or closing Preferences
  usually comes first; review L3, `Ctx::write_setting_landed`).
- File pickers: GPUI cannot filter (`PathPromptOptions` has no filter field and the Linux portal
  request sets none, gpui-pre-linux 0.3.7 `prompt_for_paths`), so the chosen file is validated
  instead. Correction to the audit: React filtered only the bundle picker (`pickBundleFile`,
  `.chairphoto`); its LUT "Import…" used the unfiltered `pickFile`, and the core refused
  anything but a parseable `.cube` (`editing::import_lut_into`), as it does under GPUI. A
  bundle pick is not refused by its name (review L6): the core's preview checks the contents,
  so a renamed real bundle is accepted from Browse as from a typed path, and a non-bundle is
  refused with the core's reason (`bundle_import::preview_error`).
- Cosmetic: the grid's video tooltip says "Video — double-click to open, then Play in system
  player" (`library::grid::video_tip`, naming the loupe's `PLAY_LABEL`), since a double-click
  opens the loupe on the poster (#97); RenderedImage shows "—" for a failed render; modules
  register in `BUNDLED_MODULES` order (the Tag graph third, after AI tagging).
