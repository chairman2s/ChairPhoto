//! The tag editor (`TagEditor.tsx`): one tag's taxonomy, as a dialog.
//!
//! - **Name:** inline rename, committed on Enter or when the field loses focus; a refused
//!   rename (a path collision) shows the reason and reverts the field. The full path is the
//!   subtitle, re-read from the tag tree so a rename shows at once.
//! - **Delete** asks once more ("Confirm delete"), then deletes the tag and its subtree and
//!   closes.
//! - **Description**, saved when the field loses focus. **Export:** "Organizational — don't
//!   export this tag as a keyword" (`tags.exportable`, docs/taxonomy.md).
//! - **Translations** (one per language, ×, add language + text; Enter adds) and **Synonyms**
//!   (per-synonym export toggle, ×, add with optional language and export; Enter adds).
//! - **Export preview:** the labels a photo with this tag would export, for the languages
//!   ticked ("default" = the canonical name), re-read after every change.
//! - Sections from enabled modules in the `tag-editor` slot; they read the tag being edited
//!   from [`ShellState::editing_tag`], which this view sets while it lives.
//!
//! Every read and write goes through [`run`] (off the UI thread, fenced against a catalog
//! switch); a write re-reads the catalog-derived state as React's `onChanged` did.

use super::state::{bind_dialog, run_as, CatalogGuard, TagsState};
use crate::modules::{panel as module_panel, ModuleRegistry, PanelSlot};
use crate::shell::style::Colors;
use crate::shell::ShellState;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::catalog::{TagTerm, TagWithCount};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, FontWeight, SharedString, Subscription, TestSupportExt as _, Window};

pub struct TagEditor {
    tags: Entity<TagsState>,
    /// The tree this dialog opened over ([`bind_dialog`]): its jobs run under it.
    guard: CatalogGuard,
    modules: Option<Entity<ModuleRegistry>>,
    /// The tag as the editor opened on it.
    pub tag: TagWithCount,
    /// The committed name (what a refused rename reverts to).
    committed_name: String,
    committed_description: String,
    pub name: Entity<InputState>,
    pub description: Entity<InputState>,
    pub exportable: bool,
    pub terms: Vec<TagTerm>,
    pub languages: Vec<String>,
    /// Ticked preview languages; `""` is the canonical name.
    pub preview_langs: Vec<String>,
    pub preview: Vec<String>,
    pub confirm_delete: bool,
    pub error: Option<String>,
    pub tr_lang: Entity<InputState>,
    pub tr_text: Entity<InputState>,
    pub syn_lang: Entity<InputState>,
    pub syn_text: Entity<InputState>,
    pub syn_export: bool,
    /// Bumped by every preview read; an older read's result is dropped.
    preview_generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for TagEditor {}

impl TagEditor {
    pub fn new(
        tags: Entity<TagsState>,
        shell: Entity<ShellState>,
        modules: Option<Entity<ModuleRegistry>>,
        tag: TagWithCount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let id = tag.tag.id;
        let name = cx.new(|cx| InputState::new(window, cx).default_value(tag.tag.name.clone()));
        let description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(
                    "What this tag means — helps disambiguate shared taxonomies and gives AI tagging context. Not exported to image files.",
                )
                .default_value(tag.tag.description.clone())
        });
        let tr_lang = cx.new(|cx| InputState::new(window, cx).placeholder("lang (e.g. nb)"));
        let tr_text = cx.new(|cx| InputState::new(window, cx).placeholder("Translated name"));
        let syn_lang = cx.new(|cx| InputState::new(window, cx).placeholder("lang (opt)"));
        let syn_text = cx.new(|cx| InputState::new(window, cx).placeholder("Synonym"));
        let (guard, bound) = bind_dialog(&tags, cx);
        let subscriptions = vec![
            bound,
            cx.subscribe_in(&name, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.rename(window, cx);
                }
            }),
            cx.subscribe(&description, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Blur) {
                    this.save_description(cx);
                }
            }),
            cx.subscribe_in(&tr_text, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.add_translation(window, cx);
                }
            }),
            cx.subscribe_in(&syn_text, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.add_synonym(window, cx);
                }
            }),
            cx.observe(&tags, |_, _, cx| cx.notify()),
            // Module sections read the tag being edited from the shell; a closed editor stops
            // naming it — only if a newer editor has not taken over.
            cx.on_release({
                let shell = shell.clone();
                move |_, cx| {
                    shell.update(cx, |s, cx| {
                        if s.editing_tag == Some(id) {
                            s.set_editing_tag(None, cx);
                        }
                    })
                }
            }),
        ];
        shell.update(cx, |s, cx| s.set_editing_tag(Some(id), cx));
        let mut this = TagEditor {
            tags,
            guard,
            modules,
            committed_name: tag.tag.name.clone(),
            committed_description: tag.tag.description.clone(),
            tag,
            name,
            description,
            exportable: true,
            terms: Vec::new(),
            languages: Vec::new(),
            preview_langs: vec![String::new()],
            preview: Vec::new(),
            confirm_delete: false,
            error: None,
            tr_lang,
            tr_text,
            syn_lang,
            syn_text,
            syn_export: true,
            preview_generation: 0,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        let id = this.id();
        // A failed read leaves the tag exportable, as React's `.catch(() => setExportable(true))`.
        run_as(&this.tags, &this.guard, cx, false, move |c| c.tag_exportable(id), |s: &mut Self, r, cx| {
            s.exportable = r.unwrap_or(true);
            s.refresh_preview(cx);
        });
        this
    }

    pub fn id(&self) -> i64 {
        self.tag.tag.id
    }

    /// Re-read the terms and the languages, then the preview.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let id = self.id();
        run_as(&self.tags, &self.guard, cx, false, move |c| Ok((c.list_terms(id)?, c.list_languages()?)), |s: &mut Self, r, cx| {
            match r {
                Ok((terms, languages)) => {
                    s.terms = terms;
                    s.languages = languages;
                }
                Err(e) => s.error = Some(e),
            }
            s.refresh_preview(cx);
            cx.notify();
        });
    }

    pub fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_generation += 1;
        let generation = self.preview_generation;
        let (id, langs) = (self.id(), self.preview_langs.clone());
        run_as(&self.tags, &self.guard, cx, false, move |c| c.export_labels(id, &langs), move |s: &mut Self, r, cx| {
            if s.preview_generation == generation {
                s.preview = r.unwrap_or_default();
                cx.notify();
            }
        });
    }

    /// A write of this editor's: on success re-read the terms (and the preview).
    fn write(&mut self, cx: &mut Context<Self>, work: impl FnOnce(&chairphoto_core::catalog::Catalog) -> chairphoto_core::catalog::Result<()> + Send + 'static) {
        run_as(&self.tags, &self.guard, cx, true, work, |s: &mut Self, r, cx| {
            if let Err(e) = r {
                s.error = Some(e);
            }
            s.reload(cx);
        });
    }

    pub fn rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.name.read(cx).value().trim().to_string();
        if next.is_empty() || next == self.committed_name {
            return;
        }
        let id = self.id();
        let attempted = next.clone();
        // The field reverts to the last committed name if the rename is refused.
        let name = self.name.clone();
        let window_handle = window.window_handle();
        run_as(&self.tags, &self.guard, cx, true, move |c| c.rename_tag(id, &next), move |s: &mut Self, r, cx| match r {
            Ok(()) => {
                s.committed_name = attempted;
                s.error = None;
                cx.notify();
            }
            Err(e) => {
                s.error = Some(e);
                let committed = s.committed_name.clone();
                window_handle
                    .update(cx, |_, window, cx| name.update(cx, |i, cx| i.set_value(committed, window, cx)))
                    .ok();
                cx.notify();
            }
        });
    }

    pub fn save_description(&mut self, cx: &mut Context<Self>) {
        let text = self.description.read(cx).value().to_string();
        if text == self.committed_description {
            return;
        }
        self.committed_description = text.clone();
        let id = self.id();
        self.write(cx, move |c| c.set_tag_description(id, &text));
    }

    pub fn set_exportable(&mut self, exportable: bool, cx: &mut Context<Self>) {
        self.exportable = exportable;
        let id = self.id();
        run_as(&self.tags, &self.guard, cx, true, move |c| c.set_tag_exportable(id, exportable), |s: &mut Self, r, cx| {
            if let Err(e) = r {
                s.error = Some(e);
            }
            s.refresh_preview(cx);
        });
        cx.notify();
    }

    pub fn add_translation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.tr_lang.read(cx).value().trim().to_string();
        let text = self.tr_text.read(cx).value().trim().to_string();
        if lang.is_empty() || text.is_empty() {
            return;
        }
        for input in [&self.tr_lang, &self.tr_text] {
            input.update(cx, |i, cx| i.set_value("", window, cx));
        }
        let id = self.id();
        self.write(cx, move |c| c.add_term(id, &text, Some(&lang), true, true).map(drop));
    }

    pub fn add_synonym(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.syn_text.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let lang = self.syn_lang.read(cx).value().trim().to_string();
        let export = self.syn_export;
        for input in [&self.syn_lang, &self.syn_text] {
            input.update(cx, |i, cx| i.set_value("", window, cx));
        }
        self.syn_export = true;
        let id = self.id();
        self.write(cx, move |c| {
            let lang = (!lang.is_empty()).then_some(lang.as_str());
            c.add_term(id, &text, lang, false, export).map(drop)
        });
    }

    pub fn remove_term(&mut self, term_id: i64, cx: &mut Context<Self>) {
        self.write(cx, move |c| c.remove_term(term_id));
    }

    pub fn set_term_export(&mut self, term_id: i64, export: bool, cx: &mut Context<Self>) {
        self.write(cx, move |c| c.set_term_export(term_id, export));
    }

    pub fn toggle_preview_lang(&mut self, lang: &str, cx: &mut Context<Self>) {
        match self.preview_langs.iter().position(|l| l == lang) {
            Some(i) => {
                self.preview_langs.remove(i);
            }
            None => self.preview_langs.push(lang.to_string()),
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Delete: the first press asks, the second deletes the tag and its subtree and closes.
    pub fn delete(&mut self, cx: &mut Context<Self>) {
        if !self.confirm_delete {
            self.confirm_delete = true;
            cx.notify();
            return;
        }
        let id = self.id();
        run_as(&self.tags, &self.guard, cx, true, move |c| c.delete_tag(id), |s: &mut Self, r, cx| match r {
            Ok(()) => cx.emit(CloseDialog),
            Err(e) => {
                s.error = Some(e);
                cx.notify();
            }
        });
    }

    fn section(title: &'static str, colors: Colors) -> gpui_kit::Div {
        div().flex().flex_col().gap(px(6.)).child(
            div().text_size(px(10.)).font_weight(FontWeight::BOLD).text_color(colors.mute).child(title.to_uppercase()),
        )
    }

    fn term_row(&self, t: &TagTerm, synonym: bool, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let term = t.id;
        let export = t.export;
        let lang = t.language.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| "—".into());
        ui::row()
            .id(SharedString::from(format!("tag-term-{term}")))
            .child(div().w(px(42.)).text_size(px(10.5)).text_color(colors.mute).child(lang))
            .child(div().flex_1().text_color(colors.txt).child(t.text.clone()))
            .when(synonym, |r| {
                r.child(super::toggle(
                    format!("tag-term-export-{term}"),
                    export,
                    "export",
                    colors,
                    cx.listener(move |s, _, _, cx| s.set_term_export(term, !export, cx)),
                ))
            })
            .child(ui::clickable(
                ui::chip(format!("tag-term-remove-{term}"), "×", true, colors),
                true,
                cx.listener(move |s, _, _, cx| s.remove_term(term, cx)),
            ))
            .test_support()
            .into_any_element()
    }
}

impl Render for TagEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let path = self.tags.read(cx).tag(self.id()).map(|t| t.tag.full_path.clone()).unwrap_or_else(|| self.tag.tag.full_path.clone());
        let module_sections = self
            .modules
            .clone()
            .and_then(|m| module_panel::render_panel_blocks(&m, PanelSlot::TagEditor, colors, window, cx));

        let header = div()
            .flex()
            .items_start()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap(px(3.))
                    .child(Input::new(&self.name).id("tag-editor-name"))
                    .child(ui::sub(path, colors))
                    .children(self.error.clone().map(|e| ui::error("tag-editor-error", e, colors))),
            )
            .child(if self.confirm_delete {
                ui::clickable(ui::danger_chip("tag-editor-delete", "Confirm delete", true, colors), true, cx.listener(|s, _, _, cx| s.delete(cx)))
            } else {
                ui::clickable(ui::chip("tag-editor-delete", "Delete", true, colors), true, cx.listener(|s, _, _, cx| s.delete(cx)))
            })
            .child(ui::clickable(ui::chip("tag-editor-close", "Close", true, colors), true, cx.listener(|_, _, _, cx| cx.emit(CloseDialog))));

        let translations: Vec<_> = self.terms.iter().filter(|t| t.is_primary).cloned().collect();
        let synonyms: Vec<_> = self.terms.iter().filter(|t| !t.is_primary).cloned().collect();
        let exportable = self.exportable;

        let mut translations_section = Self::section("Translations", colors).child(
            ui::row()
                .child(div().w(px(42.)).text_size(px(10.5)).text_color(colors.mute).child("default"))
                .child(div().flex_1().text_color(colors.txt).child(self.committed_name.clone()))
                .child(div().text_size(px(10.5)).text_color(colors.mute).child("canonical name")),
        );
        for t in &translations {
            translations_section = translations_section.child(self.term_row(t, false, colors, cx));
        }
        translations_section = translations_section.child(
            ui::row()
                .child(div().w(px(90.)).child(Input::new(&self.tr_lang).id("tag-editor-tr-lang").small()))
                .child(div().flex_1().child(Input::new(&self.tr_text).id("tag-editor-tr-text").small()))
                .child(ui::clickable(ui::chip("tag-editor-tr-add", "Add", true, colors), true, cx.listener(|s, _, w, cx| s.add_translation(w, cx)))),
        );

        let mut synonyms_section = Self::section("Synonyms", colors);
        if synonyms.is_empty() {
            synonyms_section = synonyms_section.child(ui::empty("tag-editor-no-synonyms", "No synonyms", colors));
        }
        for t in &synonyms {
            synonyms_section = synonyms_section.child(self.term_row(t, true, colors, cx));
        }
        let syn_export = self.syn_export;
        synonyms_section = synonyms_section.child(
            ui::row()
                .child(div().w(px(90.)).child(Input::new(&self.syn_lang).id("tag-editor-syn-lang").small()))
                .child(div().flex_1().child(Input::new(&self.syn_text).id("tag-editor-syn-text").small()))
                .child(super::toggle("tag-editor-syn-export", syn_export, "export", colors, cx.listener(|s, _, _, cx| {
                    s.syn_export = !s.syn_export;
                    cx.notify();
                })))
                .child(ui::clickable(ui::chip("tag-editor-syn-add", "Add", true, colors), true, cx.listener(|s, _, w, cx| s.add_synonym(w, cx)))),
        );

        let mut langs = ui::row().child(ui::sub("Languages:", colors)).child(super::toggle(
            "tag-editor-preview-default",
            self.preview_langs.iter().any(|l| l.is_empty()),
            "default",
            colors,
            cx.listener(|s, _, _, cx| s.toggle_preview_lang("", cx)),
        ));
        for lang in self.languages.clone() {
            let on = self.preview_langs.contains(&lang);
            let l = lang.clone();
            langs = langs.child(super::toggle(
                format!("tag-editor-preview-{lang}"),
                on,
                lang,
                colors,
                cx.listener(move |s, _, _, cx| s.toggle_preview_lang(&l, cx)),
            ));
        }
        let preview = div()
            .id("tag-editor-preview")
            .flex()
            .flex_wrap()
            .gap(px(5.))
            .when(self.preview.is_empty(), |p| p.child(ui::sub("nothing to export", colors)))
            .children(self.preview.iter().map(|label| {
                div().px(px(7.)).py(px(2.)).rounded(px(6.)).bg(colors.elev).text_size(px(11.5)).child(label.clone())
            }))
            .test_support();

        ui::body()
            .id("tag-editor")
            .child(header)
            .child(Self::section("Description", colors).child(Input::new(&self.description).id("tag-editor-description")))
            .child(
                Self::section("Export", colors)
                    .child(super::toggle(
                        "tag-editor-organizational",
                        !exportable,
                        "Organizational — don’t export this tag as a keyword",
                        colors,
                        cx.listener(move |s, _, _, cx| s.set_exportable(!exportable, cx)),
                    ))
                    .child(ui::sub(
                        "The tag still groups photos in your library, but it’s never written to exported files or hashtags. Tags nested under it still export.",
                        colors,
                    )),
            )
            .child(translations_section)
            .child(synonyms_section)
            .child(Self::section("Export preview", colors).child(langs).child(preview))
            .children(module_sections.map(|m| div().id("module-slot-tag-editor").child(m)))
    }
}
