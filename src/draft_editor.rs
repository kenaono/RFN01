//! Quick Draft host for the shared editor. No main window, pane registry or TAB.
use super::*;

struct Editor {
    session: editor_session::EditorSession,
    graphics: PaneGraphics,
    view: PaneView,
    timer: Timer,
    publishing: Rc<Cell<bool>>,
    refresh_pending: bool,
}

enum Action {
    Refresh(bool),
    Replaced,
    Caret(i32),
    Text(String),
    Preedit(String),
    Delete(bool),
    Move(i32, bool),
    Edge(bool, bool, bool),
    Pointer(f32, f32, i32),
    All,
    Undo(bool),
    Copy(bool),
    CopyAll,
    Escape,
    Mark(bool),
}

impl Editor {
    fn sync_text(&mut self, window: &QuickDraft) {
        let document = self.session.document();
        let incoming = window.get_text().to_string();
        if *document.text.borrow() != incoming {
            *document.text.borrow_mut() = incoming;
            *document.history.borrow_mut() = Default::default();
            *self.session.state.borrow_mut() = EditorState::default();
        }
    }

    fn layout(
        &mut self,
        window: &QuickDraft,
    ) -> windows::core::Result<(String, PaneLayout, PaneScreen)> {
        let screen = window.get_editor_screen();
        let source = self.session.document().text.borrow().clone();
        let state = self.session.state.borrow().clone();
        let typography = Typography {
            font_size: 14.0,
            // Slint 1.17 on Windows resolves the old default SansSerif to Arial.
            body_font: "Arial".into(),
            heading_font: std::array::from_fn(|_| "Arial".into()),
            code_font: "Arial".into(),
            page_margin: Some(0.0),
            ruby_room: false,
            paper: channels(window.get_editor_paper()),
            ink: channels(window.get_editor_ink()),
            ..Default::default()
        };
        let options = editor_render::LayoutOptions {
            reading: document::Reading::default(),
            preview: false,
            viewer: state.viewer,
            vertical: false,
            zoom: 100,
            scroll: screen.scroll_y,
            viewport: screen.shown_height.max(1.0),
            words: Default::default(),
            find_showing: false,
            needle: String::new(),
            rules: Default::default(),
            mark: None,
        };
        let laid_out = editor_render::layout(
            &mut self.graphics,
            &mut self.view,
            &self.session.document(),
            &source,
            LineFit::Extent(usable_horizontal_width(screen.shown_width.max(1.0))),
            &typography,
            None,
            state.caret_source_byte,
            pane_selection(&state),
            &state.preedit,
            &options,
        )?;
        Ok((source, laid_out, screen))
    }

    fn render(&mut self, window: &QuickDraft, follow: bool) -> windows::core::Result<PaneScreen> {
        let (_, laid_out, mut screen) = self.layout(window)?;
        let engine = &mut self.graphics.engine;
        screen.content_width = engine.line_extent() as i32;
        screen.content_height = engine.total_flow_size() as i32;
        let caret = laid_out
            .render_caret
            .filter(|at| engine.position_ready(*at))
            .map(|at| engine.caret_geometry(at))
            .transpose()?;
        screen.caret_visible = caret.is_some();
        if let Some(caret) = caret {
            screen.caret_x = caret.x;
            screen.caret_y = caret.y;
            screen.caret_width = caret.width;
            screen.caret_height = caret.height;
            screen.ime_anchor_x = caret.x;
            screen.ime_anchor_y = caret.y;
            screen.ime_anchor_width = caret.width;
            screen.ime_anchor_height = caret.height;
            if follow {
                let minimum = (screen.shown_height - screen.content_height as f32).min(0.0);
                let target = caret_visible_scroll(
                    screen.scroll_y,
                    screen.shown_height,
                    (minimum, 0.0),
                    caret.y,
                    caret.height,
                );
                if (target - screen.scroll_y).abs() > 0.1 {
                    screen.scroll_y = target;
                    screen.scroll_generation = screen.scroll_generation.wrapping_add(1);
                }
            }
        }
        let visible = (-screen.scroll_y, -screen.scroll_y + screen.shown_height);
        let mut rects = Vec::new();
        for run in laid_out.selection {
            for rect in engine.selection_rects(Some(run), visible)? {
                rects.push(PreviewSelectionRect {
                    x: rect.left,
                    y: rect.top,
                    width: rect.right - rect.left,
                    height: rect.bottom - rect.top,
                });
            }
        }
        screen.selection_rects = ModelRc::new(VecModel::from(rects));
        let (tiles, _) = editor_render::tiles(
            &mut self.graphics,
            self.view.preedit_range,
            editor_render::TileViewport {
                scroll: screen.scroll_y,
                shown_flow: screen.shown_height,
                scroll_across: screen.scroll_x,
                shown_across: screen.shown_width,
                vertical: false,
                shift: 0.0,
            },
            TILE_PREFETCH_COUNT,
        )?;
        screen.tiles = ModelRc::new(VecModel::from(tiles));
        let document = self.session.document();
        let history = document.history.borrow();
        screen.can_undo = !history.done.is_empty();
        screen.can_redo = !history.undone.is_empty();
        screen.ime_buffer = Default::default();
        Ok(screen)
    }

    fn insert(&mut self, text: &str) {
        let input = normalize_typed_input(text);
        if input.is_empty() {
            return;
        }
        let document = self.session.document();
        if self.session.state.borrow().rectangular {
            let removed = spliced_out(&document.text.borrow(), &self.view.selection_source);
            if let Some((start, end, kept)) = removed {
                self.session.splice(start, end, &kept, start);
            }
        }
        let source = document.text.borrow().clone();
        if !fits_document_limit(&source, &input) {
            return;
        }
        let state = self.session.state.borrow();
        let caret = floor_char_boundary(&source, state.caret_source_byte.unwrap_or(source.len()));
        let (start, end) = selection_source_range(&state).unwrap_or((caret, caret));
        drop(state);
        self.session.insert_at(source, start, end, input);
    }

    fn remove(&mut self, backwards: bool) {
        let document = self.session.document();
        let source = document.text.borrow().clone();
        if let Some((start, end, kept)) = spliced_out(&source, &self.view.selection_source) {
            self.session.splice(start, end, &kept, start);
        } else {
            let caret = floor_char_boundary(
                &source,
                self.session
                    .state
                    .borrow()
                    .caret_source_byte
                    .unwrap_or(source.len()),
            );
            let shown = PaneText::Source(&source);
            let (start, end) = if backwards {
                (shown.previous_grapheme(caret), caret)
            } else {
                (caret, shown.next_grapheme(caret))
            };
            self.session.splice(start, end, "", start);
        }
    }
}

fn run(window: &QuickDraft, held: &Rc<RefCell<Editor>>, action: Action) -> bool {
    let Ok(mut editor) = held.try_borrow_mut() else {
        return false;
    };
    if editor.publishing.get() {
        if matches!(action, Action::Refresh(_)) {
            editor.refresh_pending = true;
        }
        return false;
    }
    editor.refresh_pending = false;
    editor.sync_text(window);
    let document = editor.session.document();
    let before = document.text.borrow().clone();
    let mut follow = true;
    let mut copied = false;
    let mut handled = true;
    match action {
        Action::Refresh(keep_caret) => follow = keep_caret,
        Action::Replaced => follow = false,
        Action::Caret(at) => {
            let at = floor_char_boundary(&before, at.max(0) as usize);
            editor_interaction::select_range(&editor.session.state, &before, at, at);
        }
        Action::Text(text) => editor.insert(&text),
        Action::Preedit(text) => editor.session.state.borrow_mut().preedit = text,
        Action::Delete(backwards) => editor.remove(backwards),
        Action::Undo(forwards) => {
            editor.session.undo(forwards);
        }
        Action::All => {
            editor_interaction::select_range(&editor.session.state, &before, 0, before.len())
        }
        Action::Escape => handled = release_selection(&mut editor.session.state.borrow_mut()),
        Action::Mark(rectangle) => {
            let mut state = editor.session.state.borrow_mut();
            let at = floor_char_boundary(&before, state.caret_source_byte.unwrap_or(before.len()));
            state.mark = !state.mark || state.rectangular != rectangle;
            state.rectangular = rectangle && state.mark;
            if state.mark {
                state.selection_anchor_source_byte = Some(at);
                state.caret_source_byte = Some(at);
            }
        }
        Action::Move(direction, extend) => {
            editor.session.state.borrow_mut().preedit.clear();
            if editor.layout(window).is_ok() {
                let state = editor.session.state.borrow().clone();
                let at =
                    floor_char_boundary(&before, state.caret_source_byte.unwrap_or(before.len()));
                // Native Quick Draft collapsed an ordinary selection before stepping.
                // Keep that host policy while using the common movement implementation.
                let collapsed = (!extend && !state.mark && direction.abs() == 1)
                    .then(|| selection_source_range(&state))
                    .flatten()
                    .map(|(start, end)| if direction < 0 { start } else { end });
                let moved = if let Some(next) = collapsed {
                    Ok((next, None))
                } else {
                    editor_interaction::move_caret(
                        &mut editor.graphics.engine,
                        PaneText::Source(&before),
                        at,
                        direction,
                        state.preferred_line,
                        false,
                    )
                };
                if let Ok((next, preferred)) = moved {
                    let mut state = editor.session.state.borrow_mut();
                    update_selection_after_move(&mut state, at, next, extend);
                    state.preferred_line = preferred;
                }
            }
        }
        Action::Edge(to_end, whole, extend) => {
            editor.session.state.borrow_mut().preedit.clear();
            if editor.layout(window).is_ok() {
                let at = floor_char_boundary(
                    &before,
                    editor
                        .session
                        .state
                        .borrow()
                        .caret_source_byte
                        .unwrap_or(before.len()),
                );
                let next = if whole {
                    if to_end { before.len() } else { 0 }
                } else {
                    let edge = editor
                        .graphics
                        .engine
                        .move_caret_to_line_edge(utf16_at_byte(&before, at) as u32, to_end);
                    byte_at_utf16(&before, edge as usize)
                };
                let mut state = editor.session.state.borrow_mut();
                update_selection_after_move(&mut state, at, next, extend);
                state.preferred_line = None;
            }
        }
        Action::Pointer(x, y, phase) => {
            editor.session.state.borrow_mut().preedit.clear();
            if editor.layout(window).is_ok() && editor.graphics.engine.point_ready(x, y) {
                if let Ok(hit) = editor.graphics.engine.hit_test(x, y) {
                    let phase = match phase {
                        0 => SelectionPhase::Begin,
                        1 => SelectionPhase::Update,
                        2 => SelectionPhase::End,
                        _ => SelectionPhase::Extend,
                    };
                    let hit = PaneHit {
                        byte: byte_at_utf16(&before, hit.utf16_position as usize),
                        letter: byte_at_utf16(&before, hit.utf16_letter as usize),
                        in_numbers: editor.graphics.engine.in_number_column(x, y),
                        is_inside: hit.is_inside,
                    };
                    if let editor_interaction::PointerResult::Range(start, end) =
                        editor_interaction::pointer(
                            &editor.session.state,
                            &before,
                            hit,
                            x,
                            y,
                            phase,
                            false,
                        )
                    {
                        editor_interaction::select_range(
                            &editor.session.state,
                            &before,
                            start,
                            end,
                        );
                    }
                }
            }
        }
        Action::Copy(cut) => {
            let text = selected_text(&before, &editor.view.selection_source);
            if !text.is_empty() && clipboard::put_text(None, &text) && cut {
                editor.remove(true);
            }
            follow = false;
        }
        Action::CopyAll => {
            clipboard::put_text(None, &before);
            copied = true;
            follow = false;
        }
    }
    let rendered = editor.render(window, follow);
    let text = document.text.borrow().clone();
    let state = editor.session.state.borrow().clone();
    let publishing = editor.publishing.clone();
    publishing.set(true);
    drop(editor);
    if let Ok(screen) = rendered {
        window.set_editor_screen(screen);
    } else if let Err(error) = rendered {
        window.set_notice(format!("Editor: {error}").into());
    }
    window.set_editor_selected(selection_source_range(&state).is_some());
    window.set_caret(state.caret_source_byte.unwrap_or(0).min(i32::MAX as usize) as i32);
    if window.get_text().as_str() != text {
        window.set_text(text.clone().into());
    }
    publishing.set(false);
    if before != text {
        window.invoke_edited();
    }
    if copied {
        window.invoke_copied();
    }
    handled
}

pub(crate) fn install(window: &QuickDraft) {
    let weak = window.as_weak();
    window.on_editor_ime_area(move |x, y, w, h, vertical| {
        if let Some(window) = weak.upgrade() {
            ime::candidate_area(window.window(), x, y, w, h, vertical);
        }
    });
    let document = OpenDocument::with_events(
        DocumentFile::untitled(0),
        window.get_text().to_string(),
        Default::default(),
    );
    let held = Rc::new(RefCell::new(Editor {
        session: editor_session::EditorSession::new(document),
        graphics: PaneGraphics::new(WritingMode::Horizontal),
        view: PaneView::default(),
        timer: Timer::default(),
        publishing: Rc::new(Cell::new(false)),
        refresh_pending: false,
    }));
    macro_rules! wire {
        ($method:ident, || $action:expr) => {{
            let weak = window.as_weak(); let editor = held.clone();
            window.$method(move || { if let Some(window) = weak.upgrade() { run(&window, &editor, $action); } });
        }};
        ($method:ident, |$($arg:ident),*| $action:expr) => {{
            let weak = window.as_weak(); let editor = held.clone();
            window.$method(move |$($arg),*| { if let Some(window) = weak.upgrade() { run(&window, &editor, $action); } });
        }};
    }
    wire!(on_editor_replaced, || Action::Replaced);
    wire!(on_editor_caret, |at| Action::Caret(at));
    wire!(on_editor_resized, || Action::Refresh(false));
    wire!(on_editor_scrolled, |_position, _across| Action::Refresh(
        false
    ));
    wire!(on_editor_text, |text| Action::Text(text.to_string()));
    wire!(on_editor_preedit, |text| Action::Preedit(text.to_string()));
    wire!(on_editor_delete, |backwards| Action::Delete(backwards));
    wire!(on_editor_move, |direction, extend| Action::Move(
        direction, extend
    ));
    wire!(on_editor_edge, |to_end, whole, extend| Action::Edge(
        to_end, whole, extend
    ));
    wire!(on_editor_pointer, |x, y, phase| Action::Pointer(
        x, y, phase
    ));
    wire!(on_editor_select_all, || Action::All);
    wire!(on_editor_undo, |forwards| Action::Undo(forwards));
    wire!(on_editor_copy, |cut| Action::Copy(cut));
    wire!(on_editor_copy_all, || Action::CopyAll);
    wire!(on_editor_mark, |rectangle| Action::Mark(rectangle));
    let weak = window.as_weak();
    let editor = held.clone();
    window.on_editor_escape(move || {
        weak.upgrade()
            .is_some_and(|window| run(&window, &editor, Action::Escape))
    });
    let weak = window.as_weak();
    let editor = Rc::downgrade(&held);
    held.borrow()
        .timer
        .start(TimerMode::Repeated, Duration::from_millis(32), move || {
            if let (Some(window), Some(editor)) = (weak.upgrade(), editor.upgrade()) {
                let ready = editor.try_borrow().is_ok_and(|editor| {
                    editor.refresh_pending || editor.graphics.engine.layout_ready()
                });
                if ready {
                    run(&window, &editor, Action::Refresh(false));
                }
            }
        });
    run(window, &held, Action::Refresh(false));
}
