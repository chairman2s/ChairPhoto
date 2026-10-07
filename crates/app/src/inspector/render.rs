//! What [`PhotoInspector`] draws: the details, versions and publish tab bodies (the tags tab
//! is the shell's: `crate::tags::photo_tags::PhotoTags` and the module panels). After `PhotoInspector.tsx`'s
//! markup and App.css's `.ins-*`, `.field*`, `.stars`, `.pick-seg`, `.label-swatches`,
//! `.versions*` and `.published*` rules.

use super::*;
use crate::image_store::ImageState;
use crate::shell::actions::PublishSelection;
use crate::shell::style::{dot_ring, Colors, COLOR_LABELS};
use crate::storage::ui::{chip, clickable};
use crate::loupe::zoom::fitted;
use chairphoto_core::catalog::StorageStatus;
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, AnyElement, App, ClickEvent, Div, FontWeight, ObjectFit, SharedString, Stateful,
    TestSupportExt as _,
};

/// The Storage section's collapsed summary per status (`STORAGE_META`).
pub fn storage_meta(status: StorageStatus, colors: Colors) -> (&'static str, gpui_kit::Hsla) {
    match status {
        StorageStatus::LocalOnly => ("Local only", colors.rating),
        StorageStatus::BackedUp => ("Backed up", colors.ok),
        StorageStatus::Archived => ("On NAS", colors.accent),
        StorageStatus::Offline => ("NAS offline", colors.danger),
        StorageStatus::Missing => ("Missing", colors.danger),
    }
}

/// `.ins-label`.
fn label(text: &str, colors: Colors) -> Div {
    div().text_size(px(10.)).font_weight(FontWeight::BOLD).text_color(colors.mute).child(text.to_uppercase())
}

/// `.ins-block`.
fn block() -> Div {
    div().flex().flex_col().gap(px(6.)).px(px(14.)).py(px(8.))
}

fn empty(text: impl Into<SharedString>, colors: Colors) -> Div {
    div().text_size(px(11.)).text_color(colors.mute).child(text.into())
}

fn row() -> Div {
    div().flex().flex_row().flex_wrap().items_center().gap(px(6.))
}

type Handler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

impl PhotoInspector {
    /// Click handler that runs `f` on this entity.
    fn on(cx: &Context<Self>, f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static) -> Handler {
        let this = cx.entity().downgrade();
        Box::new(move |_, window, cx| {
            this.update(cx, |t, cx| f(t, window, cx)).ok();
        })
    }

    fn chip(
        &self,
        id: impl Into<SharedString>,
        text: impl Into<SharedString>,
        enabled: bool,
        colors: Colors,
        cx: &Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let h = Self::on(cx, f);
        clickable(chip(id, text, enabled, colors), enabled, move |e, w, cx| h(e, w, cx))
    }

    /// A collapsible section (`Section` in PhotoInspector.tsx): the caret and label toggle
    /// it; collapsed, the summary shows on the right; the body exists only while open.
    fn section(
        &self,
        section: Section,
        title: &str,
        summary: Option<AnyElement>,
        body: impl FnOnce() -> AnyElement,
        colors: Colors,
        cx: &Context<Self>,
    ) -> AnyElement {
        let open = self.section_open(section);
        let toggle = Self::on(cx, move |t, _, cx| t.toggle_section(section, cx));
        let head = div()
            .id(SharedString::from(format!("section-{}", section.id())))
            .flex()
            .items_center()
            .gap(px(6.))
            .py(px(7.))
            .cursor_pointer()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colors.dim)
            .hover(|s| s.text_color(colors.txt))
            .child(div().w(px(10.)).text_color(colors.mute).child(if open { "▾" } else { "▸" }))
            .child(div().flex_1().child(title.to_string()))
            .when(!open, |d| {
                d.children(summary.map(|s| div().text_size(px(10.5)).font_weight(FontWeight::NORMAL).text_color(colors.mute).child(s)))
            })
            .on_click(move |e, w, cx| toggle(e, w, cx))
            .test_support();
        div()
            .flex()
            .flex_col()
            .px(px(14.))
            .border_t_1()
            .border_color(colors.line)
            .child(head)
            .when(open, |d| d.child(div().pb(px(10.)).child(body())))
            .into_any_element()
    }

    // --- details -----------------------------------------------------------------------

    fn render_details(&self, photo: &Photo, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let mut primary = div().flex().flex_col();
        let exif = exif_line(photo);
        if !exif.is_empty() {
            primary = primary.child(
                block().child(
                    div()
                        .id("inspector-exif")
                        .text_size(px(11.))
                        .text_color(colors.dim)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .aria_label(exif.clone())
                        .child(exif)
                        .test_support(),
                ),
            );
        }

        let mut stars = row().gap(px(2.));
        for n in 1..=5i64 {
            let on = n <= photo.rating;
            let h = Self::on(cx, move |t, _, cx| t.rate(n, cx));
            stars = stars.child(
                div()
                    .id(SharedString::from(format!("star-{n}")))
                    .text_size(px(18.))
                    .cursor_pointer()
                    .text_color(if on { colors.rating } else { colors.border })
                    .hover(|s| s.text_color(colors.rating))
                    .child("★")
                    .on_click(move |e, w, cx| h(e, w, cx))
                    .test_support(),
            );
        }
        primary = primary.child(block().child(label("Rating", colors)).child(stars));

        let mut picks = row().gap(px(0.));
        for (state, text) in [(PickState::Pick, "Pick"), (PickState::Reject, "Reject"), (PickState::None, "None")] {
            let on = photo.pick_state == state;
            let h = Self::on(cx, move |t, _, cx| t.pick(state, cx));
            let tint = match state {
                PickState::Pick => colors.ok,
                PickState::Reject => colors.danger,
                PickState::None => colors.dim,
            };
            picks = picks.child(
                div()
                    .id(SharedString::from(format!("pick-{}", text.to_lowercase())))
                    .px(px(12.))
                    .py(px(4.))
                    .border_1()
                    .border_color(if on { tint } else { colors.border })
                    .text_size(px(11.5))
                    .text_color(if on { tint } else { colors.dim })
                    .when(on, |d| d.bg(colors.elev))
                    .cursor_pointer()
                    .child(text)
                    .on_click(move |e, w, cx| h(e, w, cx))
                    .test_support(),
            );
        }
        primary = primary.child(block().child(label("Pick", colors)).child(picks));

        let mut swatches = row().gap(px(9.));
        for l in COLOR_LABELS {
            let on = photo.label == l.name;
            let h = Self::on(cx, move |t, _, cx| t.label(l.name, cx));
            swatches = swatches.child(
                div()
                    .id(SharedString::from(format!("swatch-{}", l.name)))
                    .size(px(16.))
                    .rounded_full()
                    .bg(l.color())
                    .cursor_pointer()
                    .when(on, |d| d.shadow(dot_ring(l.color(), colors.panel)))
                    .tooltip(crate::shell::title_bar::tooltip(l.name))
                    .on_click(move |e, w, cx| h(e, w, cx))
                    .test_support(),
            );
        }
        let clear = Self::on(cx, |t, _, cx| t.label("", cx));
        swatches = swatches.child(
            div()
                .id("swatch-clear")
                .size(px(16.))
                .rounded_full()
                .border_1()
                .border_color(colors.dim)
                .cursor_pointer()
                .tooltip(crate::shell::title_bar::tooltip("Clear label"))
                .on_click(move |e, w, cx| clear(e, w, cx))
                .test_support(),
        );
        primary = primary.child(block().child(label("Color label", colors)).child(swatches));
        primary = primary.child(block().child(label("Culling signals", colors)).child(signals::render(&self.data.signals.load, colors)));

        let mut sections = div().flex().flex_col().mt(px(6.));
        if let Some(stack) = self.render_stack(photo, colors, cx) {
            sections = sections.child(stack);
        }
        sections = sections.child(self.section(
            Section::Orientation,
            "Orientation",
            None,
            || {
                row()
                    .child(self.chip("rotate-left", "↺ Left", true, colors, cx, |t, _, cx| t.rotate(-90, cx)))
                    .child(self.chip("rotate-right", "↻ Right", true, colors, cx, |t, _, cx| t.rotate(90, cx)))
                    .child(self.chip("rotate-180", "180°", true, colors, cx, |t, _, cx| t.rotate(180, cx)))
                    .into_any_element()
            },
            colors,
            cx,
        ));
        if let Some(develop) = self.render_develop(photo.id, colors, cx) {
            sections = sections.child(develop);
        }
        sections = sections.child(self.render_storage(photo.id, colors, cx));
        sections = sections.child(self.section(Section::Iptc, "IPTC", None, || self.render_iptc(colors, cx), colors, cx));
        sections = sections.child(self.section(Section::Metadata, "Metadata", None, || self.render_metadata(colors, cx), colors, cx));

        div().flex().flex_col().child(primary).child(sections).into_any_element()
    }

    fn render_stack(&self, photo: &Photo, colors: Colors, cx: &Context<Self>) -> Option<AnyElement> {
        let Some(Some(group)) = self.data.stack.load.ready() else { return None };
        let members: Vec<Photo> = std::iter::once(group.master.clone()).chain(group.children.iter().cloned()).collect();
        let summary = Some(div().child(group.children.len().to_string()).into_any_element());
        let master_id = group.master.id;
        Some(self.section(
            Section::Stack,
            "Stack",
            summary,
            || {
                let mut body = div().flex().flex_col().gap(px(4.)).child(empty(
                    "Original + its developed / derived versions — click to view.",
                    colors,
                ));
                for m in &members {
                    let active = m.id == photo.id;
                    let is_master = m.id == master_id;
                    let name = format!("{}{}", if is_master { "Original — " } else { "" }, m.path.rsplit('/').next().unwrap_or(&m.path));
                    let thumb = match self.images.read(cx).peek(m.id, ImageKind::Thumb) {
                        ImageState::Ready(l) => fitted(("stack-row-picture", m.id as u64), l.image.clone(), ObjectFit::Cover).into_any_element(),
                        _ => div().size_full().bg(colors.well).into_any_element(),
                    };
                    let mut r = div()
                        .id(SharedString::from(format!("stack-row-{}", m.id)))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .p(px(3.))
                        .rounded(px(6.))
                        .when(active, |d| d.bg(colors.sel))
                        .child(
                            div()
                                .id(("stack-row-thumb", m.id as u64))
                                .size(px(32.))
                                .flex_none()
                                .overflow_hidden()
                                .rounded(px(4.))
                                .child(thumb)
                                .test_support(),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_size(px(11.))
                                .text_color(colors.dim)
                                .child(name),
                        );
                    if !active {
                        let p = m.clone();
                        r = r.child(self.chip(
                            format!("stack-view-{}", m.id),
                            "View",
                            true,
                            colors,
                            cx,
                            move |t, _, cx| t.view_stack_member(p.clone(), cx),
                        ));
                    }
                    if !is_master {
                        let child = m.id;
                        r = r.child(self.chip(format!("stack-unstack-{}", m.id), "Unstack", true, colors, cx, move |t, _, cx| {
                            t.unstack(child, cx)
                        }));
                    }
                    body = body.child(r.test_support());
                }
                body.into_any_element()
            },
            colors,
            cx,
        ))
    }

    fn render_develop(&self, photo: i64, colors: Colors, cx: &Context<Self>) -> Option<AnyElement> {
        let editors = self.editors.as_ref()?;
        if editors.editors.is_empty() && !editors.rapidraw {
            return None;
        }
        let summary: Vec<String> = editors
            .editors
            .iter()
            .map(|e| e.label.clone())
            .chain(editors.rapidraw.then(|| "RapidRAW".to_string()))
            .collect();
        let summary = Some(div().child(summary.join(" · ")).into_any_element());
        let sidecar = self.sidecar.get(&photo).cloned();
        let rapid = self.rapid.get(&photo).copied();
        let busy = sidecar.is_some();
        let note = rapid.map(|r| r.phase.note().to_string()).or_else(|| self.notes.get(&photo).cloned());
        Some(self.section(
            Section::Develop,
            "Edit in",
            summary,
            || {
                let mut body = div().flex().flex_col().gap(px(6.));
                for e in &editors.editors {
                    let (key, label) = (e.key.clone(), e.label.clone());
                    let text = if sidecar.as_ref().is_some_and(|s| s.editor == e.key) { format!("{label}…") } else { label.clone() };
                    let mut r = row().child(self.chip(format!("edit-in-{key}"), text, !busy, colors, cx, {
                        let (key, label) = (key.clone(), label.clone());
                        move |t, _, cx| t.develop(&key, &label, cx)
                    }));
                    if e.cli {
                        let key = key.clone();
                        r = r.child(self.chip(format!("import-{key}"), "Import result", !busy, colors, cx, move |t, _, cx| {
                            t.import_result(&key, cx)
                        }));
                    }
                    body = body.child(r);
                }
                if editors.rapidraw {
                    let mut r = row().child(self.chip(
                        "edit-rapidraw",
                        if rapid.is_some() { "RapidRAW…" } else { "RapidRAW" },
                        !busy && rapid.is_none(),
                        colors,
                        cx,
                        |t, _, cx| t.edit_in_rapidraw(cx),
                    ));
                    if rapid.is_some() {
                        r = r.child(self.chip("cancel-rapidraw", "Cancel", true, colors, cx, |t, _, cx| t.cancel_rapidraw(cx)));
                    }
                    body = body.child(r);
                }
                if let Some(n) = note {
                    body = body.child(
                        div().id("develop-note").text_size(px(10.5)).text_color(colors.mute).aria_label(n.clone()).child(n).test_support(),
                    );
                }
                body.into_any_element()
            },
            colors,
            cx,
        ))
    }

    fn render_storage(&self, photo: i64, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let status = self.shell.read(cx).library.statuses().get(&photo).copied();
        // "Changed since backup" is a status of its own (#257): a backed-up photo whose local
        // version moved on from its verified backup.
        let drift = match (&self.data.drift.load, status) {
            (Load::Ready(Some(d)), Some(StorageStatus::BackedUp)) if d.changed() => Some(d.clone()),
            _ => None,
        };
        let meta = |s: StorageStatus| match &drift {
            Some(_) => ("Changed since backup", colors.rating),
            None => storage_meta(s, colors),
        };
        let summary = status.map(|s| {
            let (text, color) = meta(s);
            div().flex().items_center().gap(px(5.)).child(div().size(px(6.)).rounded_full().bg(color)).child(text).into_any_element()
        });
        let msg = self
            .storage_msg
            .clone()
            .or_else(|| drift.as_ref().and_then(chairphoto_model::storage_outcome::drift_message))
            .or_else(|| status.map(|s| meta(s).0.to_string()))
            .unwrap_or_default();
        let asking = self.replace_confirm == Some(photo);
        self.section(
            Section::Storage,
            "Storage",
            summary,
            || {
                let mut r = row();
                // Disabled while one of them runs for this photo (#254).
                let idle = !self.storage_running.contains(&photo);
                match status {
                    Some(StorageStatus::LocalOnly) => {
                        r = r.child(self.chip("storage-backup", "Back up", idle, colors, cx, |t, _, cx| t.back_up(cx)))
                    }
                    Some(StorageStatus::BackedUp) => {
                        r = r.child(self.chip("storage-offload", "Offload local", idle, colors, cx, |t, _, cx| t.offload(cx)));
                        match &drift {
                            Some(d) if d.needs_replace() && !asking => {
                                r = r.child(self.chip(
                                    "storage-replace",
                                    "Replace backup with the local version",
                                    idle,
                                    colors,
                                    cx,
                                    |t, _, cx| t.ask_replace_backup(cx),
                                ))
                            }
                            // ChairPhoto's own metadata only: Back up takes it home now.
                            Some(d) if !d.needs_replace() => {
                                r = r.child(self.chip("storage-backup", "Back up", idle, colors, cx, |t, _, cx| t.back_up(cx)))
                            }
                            _ => {}
                        }
                    }
                    Some(StorageStatus::Archived | StorageStatus::Offline) => {
                        r = r.child(self.chip("storage-restore", "Restore local", idle, colors, cx, |t, _, cx| t.restore(cx)))
                    }
                    _ => {}
                }
                let r = r.child(
                    div().id("storage-msg").text_size(px(10.5)).text_color(colors.mute).aria_label(msg.clone()).child(msg).test_support(),
                );
                match drift.as_ref().filter(|d| asking && d.needs_replace()) {
                    // The confirmation the replace needs (#257): which files, and that the copy
                    // at home is kept.
                    Some(d) => {
                        let question = chairphoto_model::storage_outcome::replace_question(d);
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(r)
                            .child(
                                div()
                                    .id("storage-replace-question")
                                    .text_size(px(10.5))
                                    .aria_label(question.clone())
                                    .child(question)
                                    .test_support(),
                            )
                            .child(
                                row()
                                    .child(self.chip("storage-replace-confirm", "Replace backup", idle, colors, cx, |t, _, cx| {
                                        t.replace_backup(cx)
                                    }))
                                    .child(self.chip("storage-replace-cancel", "Cancel", true, colors, cx, |t, _, cx| {
                                        t.cancel_replace_backup(cx)
                                    })),
                            )
                            .into_any_element()
                    }
                    None => r.into_any_element(),
                }
            },
            colors,
            cx,
        )
    }

    fn render_iptc(&self, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let field_label = |t: &str| div().text_size(px(10.5)).text_color(colors.mute).child(t.to_string());
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(field_label("Caption / Description"))
            .child(div().id("iptc-description").child(Textarea::new(&self.iptc.description)));
        for ((key, text), input) in IPTC_FIELDS.iter().zip(&self.iptc.fields) {
            body = body.child(field_label(text)).child(Input::new(input).small().id(SharedString::from(format!("iptc-{key}"))));
        }
        // Not while the photo's fields load: the form is still empty.
        let dirty = self.iptc_loaded() && self.iptc.dirty(cx);
        let status = self.iptc.status.clone();
        body.child(
            row()
                .mt(px(4.))
                .child(self.chip("iptc-save", "Save IPTC", dirty, colors, cx, |t, _, cx| t.save_iptc(cx)))
                .child(div().id("iptc-status").text_size(px(10.5)).text_color(colors.mute).aria_label(status.clone()).child(status).test_support()),
        )
        .into_any_element()
    }

    fn render_metadata(&self, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let entries = match &self.data.metadata.load {
            Load::Ready(e) if !e.is_empty() => e,
            Load::Idle | Load::Loading => return empty("Reading metadata…", colors).into_any_element(),
            _ => return empty("No metadata", colors).into_any_element(),
        };
        let mut body = div().flex().flex_col().gap(px(2.));
        for (group, items) in metadata_groups(entries) {
            let open = self.meta_open.contains(&group);
            let g = group.clone();
            let h = Self::on(cx, move |t, _, cx| t.toggle_meta_group(&g, cx));
            body = body.child(
                div()
                    .id(SharedString::from(format!("meta-group-{group}")))
                    .flex()
                    .gap(px(6.))
                    .py(px(3.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.dim)
                    .child(if open { "▾" } else { "▸" })
                    .child(group.clone())
                    .child(div().text_color(colors.mute).child(items.len().to_string()))
                    .on_click(move |e, w, cx| h(e, w, cx))
                    .test_support(),
            );
            if open {
                for e in items {
                    body = body.child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .pl(px(14.))
                            .text_size(px(10.5))
                            .child(div().w(px(110.)).flex_none().overflow_hidden().text_ellipsis().text_color(colors.mute).child(e.key.clone()))
                            .child(div().flex_1().min_w_0().text_color(colors.dim).child(e.value.clone())),
                    );
                }
            }
        }
        body.into_any_element()
    }

    // --- versions ----------------------------------------------------------------------

    fn render_versions(&self, colors: Colors, window: &mut Window, cx: &Context<Self>) -> AnyElement {
        let _ = window;
        let active = self.shell.read(cx).active_version().map(|v| v.id);
        let versions: Vec<PhotoVersion> = self.data.versions.load.ready().cloned().unwrap_or_default();
        let version_row = |id: SharedString, on: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .py(px(4.))
                .rounded(px(6.))
                .when(on, |d| d.bg(colors.sel))
        };
        let original = Self::on(cx, |t, _, cx| t.select_version(None, cx));
        let mut list = div().flex().flex_col().gap(px(2.)).child(
            version_row("version-original".into(), active.is_none())
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(colors.txt)
                .child("Original")
                .on_click(move |e, w, cx| original(e, w, cx))
                .test_support(),
        );
        for v in &versions {
            let renaming = self.renaming.as_ref().filter(|r| r.0 == v.id).map(|r| r.1.clone());
            let mut r = version_row(format!("version-{}", v.id).into(), active == Some(v.id));
            r = match renaming {
                Some(input) => {
                    let this = cx.entity().downgrade();
                    r.child(
                        div()
                            .flex_1()
                            .capture_key_down(move |event, _, cx| {
                                if event.keystroke.key == "escape" {
                                    this.update(cx, |t, cx| t.cancel_rename(cx)).ok();
                                    cx.stop_propagation();
                                }
                            })
                            .child(Input::new(&input).small().id("version-rename")),
                    )
                }
                None => {
                    let version = v.clone();
                    let this = cx.entity().downgrade();
                    r.child(
                        div()
                            .id(SharedString::from(format!("version-name-{}", v.id)))
                            .flex_1()
                            .min_w_0()
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(colors.txt)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(v.name.clone())
                            .tooltip(crate::shell::title_bar::tooltip("Click to view · double-click to rename"))
                            .on_click(move |e: &ClickEvent, window, cx| {
                                let version = version.clone();
                                this.update(cx, |t, cx| {
                                    if e.click_count() >= 2 {
                                        t.start_rename(&version, window, cx);
                                    } else {
                                        t.select_version(Some(version), cx);
                                    }
                                })
                                .ok();
                            })
                            .test_support(),
                    )
                }
            };
            if cfg!(feature = "edit") {
                let version = v.clone();
                r = r.child(self.chip(format!("version-edit-{}", v.id), "✎", true, colors, cx, move |t, _, cx| {
                    t.edit_version(version.clone(), cx)
                }));
            }
            let id = v.id;
            r = r
                .child(self.chip(format!("version-dup-{id}"), "⧉", true, colors, cx, move |t, _, cx| t.duplicate_version(id, cx)))
                .child(self.chip(format!("version-del-{id}"), "✕", true, colors, cx, move |t, _, cx| t.delete_version(id, cx)));
            list = list.child(r.test_support());
        }
        let add = Self::on(cx, |t, window, cx| t.add_version(window, cx));
        block()
            .child(label("Versions", colors))
            .child(list)
            .child(
                row()
                    .flex_nowrap()
                    .child(div().flex_1().child(Input::new(&self.version_name).small().id("version-new")))
                    .child(clickable(chip("version-add", "+ Add", true, colors), true, move |e, w, cx| add(e, w, cx))),
            )
            .into_any_element()
    }

    // --- publish -----------------------------------------------------------------------

    fn render_publish(&self, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let pubs: Vec<Publication> = self.data.publications.load.ready().cloned().unwrap_or_default();
        let versions: Vec<PhotoVersion> = self.data.versions.load.ready().cloned().unwrap_or_default();
        let mut list = div().flex().flex_col().gap(px(2.));
        for p in &pubs {
            let id = p.id;
            let line = format!(
                "{} · {}",
                titlecase(&p.platform),
                p.version_name.clone().unwrap_or_else(|| "Original".into())
            );
            list = list.child(
                div()
                    .id(SharedString::from(format!("publication-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(4.))
                    .child(div().flex_1().text_size(px(12.)).text_color(colors.txt).child(line))
                    .when(p.published_at != 0, |d| {
                        d.child(div().text_size(px(10.5)).text_color(colors.mute).child(publication_date(p.published_at)))
                    })
                    .child(self.chip(format!("publication-del-{id}"), "✕", true, colors, cx, move |t, _, cx| {
                        t.delete_publication(id, cx)
                    }))
                    .test_support(),
            );
        }
        if pubs.is_empty() {
            list = list.child(empty("Not published yet", colors).id("publications-empty").test_support());
        }

        let mut suggestions = row();
        for platform in COMMON_PLATFORMS {
            suggestions = suggestions.child(self.chip(format!("platform-{platform}"), platform, true, colors, cx, move |t, window, cx| {
                t.set_platform(platform, window, cx)
            }));
        }
        let chosen = self
            .publish_version
            .and_then(|id| versions.iter().find(|v| v.id == id))
            .map_or("Original".to_string(), |v| v.name.clone());
        let this = cx.entity().downgrade();
        let menu_versions = versions.clone();
        let current = self.publish_version;
        let version_menu = Button::new("publish-version-menu")
            .outline()
            .small()
            .label(chosen)
            .tooltip("Which version was published")
            .dropdown_menu(move |menu, _, _| {
                let pick = |id: Option<i64>| {
                    let this = this.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        this.update(cx, |t, cx| t.set_publish_version(id, cx)).ok();
                    }
                };
                let mut menu = menu.item(PopupMenuItem::new("Original").checked(current.is_none()).on_click(pick(None)));
                for v in &menu_versions {
                    menu = menu.item(PopupMenuItem::new(v.name.clone()).checked(current == Some(v.id)).on_click(pick(Some(v.id))));
                }
                menu
            });
        let has_photo = self.photo_id.is_some();
        block()
            .child(label("Published to", colors))
            .child(list)
            .child(div().mt(px(6.)).text_size(px(10.5)).text_color(colors.mute).child("Mark as published"))
            .child(Input::new(&self.platform).small().id("publish-platform"))
            .child(suggestions)
            .child(
                row()
                    .child(version_menu)
                    .child(self.chip("publish-mark", "+ Mark", true, colors, cx, |t, _, cx| t.mark_published(cx))),
            )
            .child(
                row().mt(px(8.)).child(clickable(
                    chip("publish-open", "Publish…", has_photo, colors),
                    has_photo,
                    |_, window, cx| window.dispatch_action(Box::new(PublishSelection), cx),
                )),
            )
            .into_any_element()
    }
}

/// `titlecase` in PublishedPanel: the platform's first letter upper-cased.
pub fn titlecase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

impl Render for PhotoInspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.iptc.apply_fill(window, cx);
        let colors = Colors::get(cx);
        let body: Stateful<Div> = div().id("photo-inspector").flex().flex_col().pb(px(16.));
        let Some(photo) = self.photo(cx) else {
            return body.child(div().p(px(16.)).child(empty("Select a photo", colors))).into_any_element();
        };
        // The stack's thumbnails, through the image layer like the grid's.
        if let Some(Some(group)) = self.data.stack.load.ready() {
            if self.section_open(Section::Stack) {
                let wanted: Vec<(i64, ImageKind)> = std::iter::once(group.master.id)
                    .chain(group.children.iter().map(|c| c.id))
                    .map(|id| (id, ImageKind::Thumb))
                    .collect();
                self.images.update(cx, |s, _| s.request_batch(&wanted));
            }
        }
        let tab = self.shell.read(cx).inspector_tab;
        let content = match tab {
            InspectorTab::Details => self.render_details(&photo, colors, cx),
            InspectorTab::Versions => self.render_versions(colors, window, cx),
            InspectorTab::Publish => self.render_publish(colors, cx),
            // The shell draws the tags tab (the Tag panel's slot and the module panels).
            InspectorTab::Tags => div().into_any_element(),
        };
        body.child(content).into_any_element()
    }
}
