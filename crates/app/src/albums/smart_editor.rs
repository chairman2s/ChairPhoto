//! "New smart album" / "Edit smart album" (`SmartAlbumEditor.tsx`): a name; AND-only
//! condition rows ([`super::rule`]) — a field menu grouped as React's `<optgroup>`s, the
//! field's operators, and a typed value (number, text, `YYYY-MM-DD` date, a pair for
//! "between", or a menu: tag by full path with counts, import batch, enum); ✕ removes a row,
//! "＋ Add condition" adds one. The live match count re-runs 300 ms after the last change.
//! Save rule / Create writes, then selects the album as the Library's scope and closes.
//!
//! Bound to the catalog its smart-album list came from ([`super::state::bind_dialog`]): the
//! tags and batches it offers, the count and the write all run under that identity, and it
//! closes on a switch.

use super::rule::{build_rule_json, enum_values, field_def, parse_rule_json, Condition, Op, ValueKind, FIELDS, GROUPS};
use super::state::{bind_dialog, run_bound, AlbumsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::app::CatalogIdentity;
use chairphoto_core::catalog::{ImportBatch, SmartAlbum, TagWithCount};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, SharedString, Subscription, Task, Window};
use std::time::Duration;

/// The live count's debounce (React's 300 ms).
pub const COUNT_DEBOUNCE: Duration = Duration::from_millis(300);

/// One condition row: the model and the two text fields its value widgets use.
pub struct Row {
    pub cond: Condition,
    pub inputs: [Entity<InputState>; 2],
    _subs: Vec<Subscription>,
}

pub struct SmartAlbumEditor {
    albums: Entity<AlbumsState>,
    from: CatalogIdentity,
    /// The album being edited, or `None` for a new one.
    pub album: Option<SmartAlbum>,
    pub name: Entity<InputState>,
    pub rows: Vec<Row>,
    pub tags: Vec<TagWithCount>,
    pub batches: Vec<ImportBatch>,
    /// The live count: `None` = unknown ("—").
    pub count: Option<i64>,
    pub counting: bool,
    count_seq: u64,
    _count_task: Option<Task<()>>,
    pub busy: bool,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for SmartAlbumEditor {}

impl SmartAlbumEditor {
    pub fn new(
        albums: Entity<AlbumsState>,
        from: CatalogIdentity,
        album: Option<SmartAlbum>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. Keepers ≥4★ this month")
                .default_value(album.as_ref().map(|a| a.name.clone()).unwrap_or_default())
        });
        let mut subs = bind_dialog(&albums, Some(from), |s| s.lists_from(), cx);
        subs.push(cx.subscribe(&name, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.error = None;
                cx.notify();
            }
        }));
        let mut editor = SmartAlbumEditor {
            albums: albums.clone(),
            from,
            album: album.clone(),
            name,
            rows: Vec::new(),
            tags: Vec::new(),
            batches: Vec::new(),
            count: None,
            counting: false,
            count_seq: 0,
            _count_task: None,
            busy: false,
            error: None,
            _subscriptions: subs,
        };
        for cond in album.as_ref().map(|a| parse_rule_json(&a.rule_json)).unwrap_or_default() {
            editor.push_row(cond, window, cx);
        }
        // The pickers' lists, from the catalog the album list came from.
        run_bound(
            &albums,
            from,
            cx,
            false,
            |c| {
                let mut tags = c.list_tags_with_counts()?;
                tags.sort_by(|a, b| a.tag.full_path.cmp(&b.tag.full_path));
                Ok((tags, c.list_import_batches()?))
            },
            |s: &mut Self, r, cx| {
                if let Ok((tags, batches)) = r {
                    (s.tags, s.batches) = (tags, batches);
                    cx.notify();
                }
            },
        );
        editor.recount(cx);
        editor
    }

    fn push_row(&mut self, cond: Condition, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = [0, 1].map(|i| {
            let value = cond.value[i].clone();
            cx.new(|cx| InputState::new(window, cx).default_value(value))
        });
        // Typing re-counts; the text itself is read from the field when the rule is built.
        let subs = inputs
            .iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.recount(cx);
                    }
                })
            })
            .collect();
        self.rows.push(Row { cond, inputs, _subs: subs });
    }

    /// The rows as conditions: a typed value (number, text, date, either side of "between")
    /// is its field's text now; a menu value is the row's own.
    pub fn conditions(&self, cx: &gpui_kit::App) -> Vec<Condition> {
        self.rows
            .iter()
            .map(|r| {
                let mut c = r.cond.clone();
                let typed = c.op == Op::Between
                    || matches!(c.def().kind, ValueKind::Int | ValueKind::Real | ValueKind::Text | ValueKind::Date);
                if typed && c.op != Op::IsSet {
                    for (v, input) in c.value.iter_mut().zip(&r.inputs) {
                        *v = input.read(cx).value().to_string();
                    }
                }
                c
            })
            .collect()
    }

    /// The rule JSON the rows make now.
    pub fn rule_json(&self, cx: &gpui_kit::App) -> String {
        build_rule_json(&self.conditions(cx))
    }

    /// "＋ Add condition": a row on the first field.
    pub fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.push_row(Condition::new(&FIELDS[0]), window, cx);
        self.recount(cx);
    }

    /// ✕ on row `i`.
    pub fn remove_row(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.rows.len() {
            self.rows.remove(i);
            self.recount(cx);
        }
    }

    /// Row `i`'s field menu: its first operator and a fresh value.
    pub fn set_field(&mut self, i: usize, field: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(def) = field_def(field) else { return };
        if let Some(row) = self.rows.get_mut(i) {
            row.cond.set_field(def);
            Self::sync_inputs(row, window, cx);
            self.recount(cx);
        }
    }

    /// Row `i`'s operator menu: a fresh value.
    pub fn set_op(&mut self, i: usize, op: Op, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(i) {
            row.cond.set_op(op);
            Self::sync_inputs(row, window, cx);
            self.recount(cx);
        }
    }

    /// Row `i`'s menu value (enum, tag or batch).
    pub fn set_choice(&mut self, i: usize, value: String, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(i) {
            row.cond.value[0] = value;
            self.recount(cx);
        }
    }

    fn sync_inputs(row: &mut Row, window: &mut Window, cx: &mut Context<Self>) {
        for (input, value) in row.inputs.iter().zip(row.cond.value.clone()) {
            input.update(cx, |i, cx| i.set_value(value, window, cx));
        }
    }

    /// Re-count after [`COUNT_DEBOUNCE`] of quiet; a newer change supersedes a pending count,
    /// and a superseded count's answer is dropped.
    fn recount(&mut self, cx: &mut Context<Self>) {
        self.count_seq += 1;
        let seq = self.count_seq;
        self.counting = true;
        let rule = self.rule_json(cx);
        let timer = cx.background_executor().timer(COUNT_DEBOUNCE);
        self._count_task = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |s, cx| {
                if s.count_seq != seq {
                    return;
                }
                let albums = s.albums.clone();
                run_bound(&albums, s.from, cx, false, move |c| c.smart_album_count(&rule), move |s: &mut Self, r, cx| {
                    if s.count_seq == seq {
                        s.counting = false;
                        s.count = r.ok();
                        cx.notify();
                    }
                });
            })
            .ok();
        }));
        cx.notify();
    }

    /// Save rule / Create: write, select the album as the scope, close.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.error = Some("Give the smart album a name.".into());
            cx.notify();
            return;
        }
        if self.busy {
            return;
        }
        self.busy = true;
        let rule = self.rule_json(cx);
        let existing = self.album.as_ref().map(|a| (a.id, a.name.clone()));
        let albums = self.albums.clone();
        run_bound(
            &albums,
            self.from,
            cx,
            true,
            move |c| match existing {
                Some((id, old)) => {
                    c.set_smart_album_rule(id, &rule)?;
                    if name != old {
                        c.rename_smart_album(id, &name)?;
                    }
                    Ok(id)
                }
                None => c.create_smart_album(&name, &rule),
            },
            |s: &mut Self, r, cx| {
                s.busy = false;
                match r {
                    Ok(id) => {
                        let shell = s.albums.read(cx).shell().clone();
                        shell.update(cx, |sh, cx| sh.update_scope(cx, |l| l.select_smart_album(Some(id))));
                        cx.emit(CloseDialog);
                    }
                    Err(e) => s.error = Some(e),
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn choice_label(&self, cond: &Condition) -> String {
        let v = cond.value[0].as_str();
        match cond.def().kind {
            ValueKind::Tag => match v.parse::<i64>().ok().and_then(|id| self.tags.iter().find(|t| t.tag.id == id)) {
                Some(t) => format!("{} ({})", t.tag.full_path, t.photo_count),
                None if v.is_empty() => "Pick a tag…".into(),
                None => format!("Tag #{v}"),
            },
            ValueKind::Batch => match v.parse::<i64>().ok().and_then(|id| self.batches.iter().find(|b| b.id == id)) {
                Some(b) => format!("{} ({})", b.source_label, b.photo_count),
                None if v.is_empty() => "Pick a batch…".into(),
                None => format!("Batch #{v}"),
            },
            _ => enum_values(cond.field).into_iter().find(|(val, _)| *val == v).map_or(v.to_string(), |(_, l)| l.to_string()),
        }
    }

    fn render_row(&self, i: usize, cond: &Condition, inputs: &[Entity<InputState>; 2], colors: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let def = cond.def();
        let field_menu = {
            let this = this.clone();
            Button::new(SharedString::from(format!("sa-field-{i}"))).outline().small().label(def.label).dropdown_menu(
                move |menu: PopupMenu, _, _| {
                    let mut menu = menu;
                    for group in GROUPS {
                        menu = menu.label(group);
                        for f in FIELDS.iter().filter(|f| f.group == group) {
                            let this = this.clone();
                            menu = menu.item(PopupMenuItem::new(f.label).on_click(move |_, window, cx| {
                                this.update(cx, |e, cx| e.set_field(i, f.field, window, cx)).ok();
                            }));
                        }
                    }
                    menu
                },
            )
        };
        let op_menu = {
            let this = this.clone();
            Button::new(SharedString::from(format!("sa-op-{i}"))).outline().small().label(cond.op.label()).dropdown_menu(
                move |menu: PopupMenu, _, _| {
                    def.ops.iter().fold(menu, |menu, &op| {
                        let this = this.clone();
                        menu.item(PopupMenuItem::new(op.label()).on_click(move |_, window, cx| {
                            this.update(cx, |e, cx| e.set_op(i, op, window, cx)).ok();
                        }))
                    })
                },
            )
        };
        let value: gpui_kit::AnyElement = if cond.op == Op::IsSet {
            ui::sub("(any value)", colors).into_any_element()
        } else if cond.op == Op::Between {
            ui::row()
                .child(div().w(px(90.)).child(Input::new(&inputs[0]).small()))
                .child(ui::sub("and", colors))
                .child(div().w(px(90.)).child(Input::new(&inputs[1]).small()))
                .into_any_element()
        } else {
            match def.kind {
                ValueKind::Tag | ValueKind::Batch | ValueKind::Enum => {
                    let choices: Vec<(String, String)> = match def.kind {
                        ValueKind::Tag => self
                            .tags
                            .iter()
                            .map(|t| (t.tag.id.to_string(), format!("{} ({})", t.tag.full_path, t.photo_count)))
                            .collect(),
                        ValueKind::Batch => {
                            self.batches.iter().map(|b| (b.id.to_string(), format!("{} ({})", b.source_label, b.photo_count))).collect()
                        }
                        _ => enum_values(def.field).into_iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
                    };
                    let this = this.clone();
                    Button::new(SharedString::from(format!("sa-value-{i}")))
                        .outline()
                        .small()
                        .label(self.choice_label(cond))
                        .dropdown_menu(move |menu: PopupMenu, _, _| {
                            choices.iter().fold(menu.max_h(px(320.)).scrollable(true), |menu, (v, l)| {
                                let (this, v) = (this.clone(), v.clone());
                                menu.item(PopupMenuItem::new(l.clone()).on_click(move |_, _, cx| {
                                    this.update(cx, |e, cx| e.set_choice(i, v.clone(), cx)).ok();
                                }))
                            })
                        })
                        .into_any_element()
                }
                _ => div().w(px(180.)).child(Input::new(&inputs[0]).small()).into_any_element(),
            }
        };
        ui::row()
            .id(SharedString::from(format!("sa-row-{i}")))
            .child(field_menu)
            .child(op_menu)
            .child(value)
            .child(ui::clickable(
                ui::chip(SharedString::from(format!("sa-remove-{i}")), "✕", true, colors),
                true,
                cx.listener(move |s, _, _, cx| s.remove_row(i, cx)),
            ))
    }
}

impl Render for SmartAlbumEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let rows: Vec<(Condition, [Entity<InputState>; 2])> =
            self.rows.iter().map(|r| (r.cond.clone(), r.inputs.clone())).collect();
        let mut conditions = ui::body().id("sa-conditions").gap(px(6.));
        if rows.is_empty() {
            conditions = conditions.child(ui::sub("No conditions — this matches every photo. Add one to narrow it.", colors));
        }
        for (i, (cond, inputs)) in rows.iter().enumerate() {
            conditions = conditions.child(self.render_row(i, cond, inputs, colors, cx));
        }
        let count = if self.counting {
            "Counting…".to_string()
        } else {
            match self.count {
                None => "—".into(),
                Some(n) => format!("{n} photo{} match", if n == 1 { "" } else { "s" }),
            }
        };
        let label = match (self.busy, &self.album) {
            (true, _) => "Saving…",
            (false, Some(_)) => "Save rule",
            (false, None) => "Create",
        };
        ui::body()
            .id("smart-album-editor")
            .child(ui::label("Name", colors))
            .child(Input::new(&self.name).id("sa-name"))
            .child(ui::label("Conditions — all must match (AND)", colors))
            .child(conditions)
            .child(ui::row().child(ui::clickable(
                ui::chip("sa-add", "＋ Add condition", true, colors),
                true,
                cx.listener(|s, _, window, cx| s.add_row(window, cx)),
            )))
            .child(
                ui::row()
                    .child(div().id("sa-count").flex_1().child(ui::sub(count, colors)))
                    .child(ui::clickable(ui::primary("sa-save", label, !self.busy, colors), !self.busy, cx.listener(|s, _, _, cx| s.save(cx)))),
            )
            .children(self.error.clone().map(|e| ui::error("sa-error", e, colors)))
    }
}
