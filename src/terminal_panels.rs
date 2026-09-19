//! RFN01-20: tab-owned lower panels and fixed log destinations.
use super::*;
use crate::buffer::ExternalChange;

impl std::fmt::Debug for PanelDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelDocument").finish_non_exhaustive()
    }
}

pub(crate) struct PanelDocument {
    pub document: Rc<OpenDocument>,
    pub shell: Option<Rc<RefCell<TerminalSession>>>,
    /// Same source-view ReadOnly state as EditorState::viewer.
    pub view: EditorState,
    pub style: Option<PanelStyle>,
    pub capture: Option<Rc<RefCell<TerminalSession>>>,
    before_read_only: bool,
    pending_tail: usize,
    history_limit: usize,
    log_name: Option<String>,
    save_tail_on_close: bool,
}

impl PanelDocument {
    fn new(
        window: &AppWindow,
        document: Rc<OpenDocument>,
        shell: Option<Rc<RefCell<TerminalSession>>>,
    ) -> Self {
        let style = terminal_appearance::random_style(window, if shell.is_some() { 2 } else { 1 });
        Self {
            document,
            shell,
            style,
            view: EditorState::default(),
            capture: None,
            before_read_only: false,
            pending_tail: 0,
            history_limit: window.get_terminal_history_limit().max(0) as usize,
            log_name: None,
            save_tail_on_close: false,
        }
    }

    pub fn stop(&mut self) {
        self.collect();
        if let Some(session) = self.capture.take() {
            session.borrow_mut().stop_capture();
            self.view.set_read_only(self.before_read_only, true);
            self.pending_tail = 0;
        }
    }

    fn collect(&mut self) -> bool {
        let Some(session) = &self.capture else {
            return false;
        };
        let mut session = session.borrow_mut();
        session.drain();
        let Some((completed, pending)) = session.capture_update() else {
            return false;
        };
        let old = self.document.text.borrow();
        let keep = old.len().saturating_sub(self.pending_tail);
        let changed = !completed.is_empty()
            || old[keep..] != pending
            || old.lines().count() > self.history_limit;
        self.pending_tail = pending.len();
        drop(old);
        if changed {
            let mut text = self.document.text.borrow_mut();
            text.truncate(keep);
            text.push_str(&completed);
            text.push_str(&pending);
            trim_log(&mut text, self.history_limit);
            self.pending_tail = self.pending_tail.min(text.len());
        }
        changed
    }
}

/// Keep the newest logical lines, including an unfinished last line.
fn trim_log(text: &mut String, limit: usize) {
    if limit == 0 {
        text.clear();
        return;
    }
    let lines = text.bytes().filter(|b| *b == b'\n').count()
        + usize::from(!text.is_empty() && !text.ends_with('\n'));
    if let Some(remove) = lines.checked_sub(limit).filter(|n| *n > 0) {
        if let Some((at, _)) = text.match_indices('\n').nth(remove - 1) {
            text.drain(..at + 1);
        }
    }
}

pub(crate) fn next_number(live: &Live) -> u32 {
    let tabs = live.tabs.borrow();
    let mut taken = Vec::new();
    for strip in &tabs.panes {
        for tab in &strip.tabs {
            taken.push(tab.document.file.borrow().untitled_number());
            for panel in &tab.below.entries {
                taken.push(panel.borrow().document.file.borrow().untitled_number());
            }
        }
    }
    next_untitled_number(&taken)
}

pub(crate) fn current(live: &Live, id: PaneId) -> Option<Rc<RefCell<PanelDocument>>> {
    let tabs = live.tabs.borrow();
    let tab = tabs.of(id).current()?;
    tab.below.entries.get(tab.below.active).cloned()
}

pub(crate) fn ensure(window: &AppWindow, live: &Live, id: PaneId) {
    let needs = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .is_some_and(|t| t.below.entries.is_empty());
    if !needs {
        return;
    }
    let doc = new_document(live);
    let mut tabs = live.tabs.borrow_mut();
    let strip = tabs.of_mut(id);
    let active = strip.active;
    if let Some(tab) = strip.tabs.get_mut(active) {
        if !tab.below.draft.is_empty() {
            *doc.text.borrow_mut() = tab.below.draft.clone();
        }
        tab.below
            .entries
            .push(Rc::new(RefCell::new(PanelDocument::new(
                window,
                doc,
                tab.below.shell.clone(),
            ))));
        tab.below.active = 0;
    }
}

fn new_document(live: &Live) -> Rc<OpenDocument> {
    // Panel state is published by edited/show/drain. SharedText's window
    // callback belongs to upper documents: firing it during a panel edit
    // re-enters Tabs (sync_entry) or the captured terminal (collect).
    // Keep dirty tracking, but do not change the upper document's UI state.
    OpenDocument::untitled(next_number(live), slint::Weak::default())
}

pub(crate) fn sync_entry(tab: &mut PaneTab) {
    if let Some(entry) = tab.below.entries.get(tab.below.active) {
        let mut entry = entry.borrow_mut();
        entry.shell = tab.below.shell.clone();
        if tab.terminal.is_some()
            && !entry.view.viewer
            && *entry.document.text.borrow() != tab.below.draft
        {
            *entry.document.text.borrow_mut() = tab.below.draft.clone();
        }
    }
}

pub(crate) fn publish(window: &AppWindow, id: PaneId, below: &TabBelow) {
    terminal_appearance::publish(window, id, below);
    let names: Vec<SharedString> = below
        .entries
        .iter()
        .map(|entry| {
            let entry = entry.borrow();
            let mut title = entry.shell.as_ref().map_or_else(
                || entry.document.file.borrow().title(),
                |s| s.borrow().title(),
            );
            if entry.document.text.edited() {
                title.push('*');
            }
            if entry.capture.is_some() {
                title.push_str(" ● Panel");
            }
            title.into()
        })
        .collect();
    let entry = below.entries.get(below.active).map(|p| p.borrow());
    let file_names: Vec<SharedString> = below
        .entries
        .iter()
        .map(|e| e.borrow().document.file.borrow().title().into())
        .collect();
    let screen = id.screen(window);
    let read_only = entry.as_ref().is_some_and(|e| e.view.viewer);
    let follow = entry.as_ref().is_some_and(|e| e.view.follow);
    let capturing = entry.as_ref().is_some_and(|e| e.capture.is_some());
    let names_changed = screen.panel_tabs.iter().ne(names.iter().cloned())
        || screen
            .panel_file_names
            .iter()
            .ne(file_names.iter().cloned());
    if !names_changed
        && screen.panel_active == below.active as i32
        && screen.panel_read_only == read_only
        && screen.panel_follow == follow
        && screen.panel_capturing == capturing
    {
        return;
    }
    id.update_screen(window, |screen| {
        if names_changed {
            screen.panel_tabs = ModelRc::new(VecModel::from(names));
            screen.panel_file_stems = ModelRc::new(VecModel::from(
                file_names
                    .iter()
                    .map(|name| stem_length(name))
                    .collect::<Vec<_>>(),
            ));
            screen.panel_file_names = ModelRc::new(VecModel::from(file_names));
        }
        screen.panel_active = below.active as i32;
        screen.panel_read_only = read_only;
        screen.panel_follow = follow;
        screen.panel_capturing = capturing;
    });
}

fn show(window: &AppWindow, live: &Live, id: PaneId) {
    let below = {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        let active = strip.active;
        let Some(tab) = strip.tabs.get_mut(active) else {
            return;
        };
        if let Some(entry) = tab.below.entries.get(tab.below.active) {
            let entry = entry.borrow();
            tab.below.draft = entry.document.text.borrow().clone();
            tab.below.shell = entry.shell.clone();
        }
        tab.below.clone()
    };
    let (kind, height) = {
        let mut cache = live.cache.borrow_mut();
        let pane = cache.pane(id);
        pane.below_open = true;
        pane.below = below.shell.as_ref().map(TerminalView::sharing);
        (below_kind(pane), pane.below_height)
    };
    show_draft(window, id, &below.draft);
    id.set_below(window, kind, height);
    publish(window, id, &below);
    publish_source(window, live, id);
    if kind == 1 {
        refresh_terminal(window, &live.cache, id, TerminalSpot::Below);
    }
}

pub(crate) fn action(window: &AppWindow, live: &Live, id: PaneId, action: i32, index: i32) {
    // File capture and source Stop must never create or open a Panel.
    if action == 10 {
        start_file(window, live, id);
        publish_source(window, live, id);
        return;
    }
    if action == 13 || action == 14 {
        stop_source(window, live, id, action == 14);
        return;
    }
    ensure(window, live, id);
    store_below_on_tab(window, live, id);
    match action {
        0 => {
            let mut tabs = live.tabs.borrow_mut();
            let strip = tabs.of_mut(id);
            let active = strip.active;
            if let Some(tab) = strip.tabs.get_mut(active) {
                if index >= 0 && (index as usize) < tab.below.entries.len() {
                    tab.below.active = index as usize;
                }
            }
        }
        1 => {
            let is_terminal = live
                .tabs
                .borrow()
                .of(id)
                .current()
                .is_some_and(|t| t.terminal.is_some());
            let shell = if is_terminal {
                None
            } else {
                let height = live.cache.borrow_mut().pane(id).below_height;
                let Some(s) = start_shell(
                    window,
                    live,
                    id,
                    &shell_at(window, window.get_default_shell()),
                    height,
                ) else {
                    return;
                };
                Some(Rc::new(RefCell::new(s)))
            };
            let doc = new_document(live);
            let entry = Rc::new(RefCell::new(PanelDocument::new(window, doc, shell)));
            let mut tabs = live.tabs.borrow_mut();
            let strip = tabs.of_mut(id);
            let active = strip.active;
            if let Some(tab) = strip.tabs.get_mut(active) {
                tab.below.entries.push(entry);
                tab.below.active = tab.below.entries.len() - 1;
            }
        }
        2 => {
            request_close(window, live, id);
            return;
        }
        3 => {
            if let Some(entry) = current(live, id) {
                save(window, live, &entry, false);
            }
        }
        4 => {
            if let Some(entry) = current(live, id) {
                let mut entry = entry.borrow_mut();
                if entry.capture.is_some() {
                    entry.stop();
                    entry.view.set_read_only(false, true);
                } else {
                    let next = !entry.view.viewer;
                    entry.view.set_read_only(next, true);
                }
            }
        }
        5 => {
            if let Some(entry) = current(live, id) {
                entry.borrow_mut().stop();
            }
        }
        6 => {
            if let Some(entry) = current(live, id) {
                save(window, live, &entry, true);
            }
        }
        7 | 8 | 9 => {
            import(window, live, id, action);
            return;
        }
        10 => {
            start_file(window, live, id);
            return;
        }
        12 => {
            let Some(entry) = current(live, id) else {
                return;
            };
            let shell = shell_at(window, index);
            let old = entry.borrow().shell.clone();
            let Some(old) = old else {
                return;
            };
            if old.borrow().name() == shell.name {
                return;
            }
            if window.get_terminal_confirm_close() && !old.borrow().finished() {
                ask_question(window, live, Question::PanelSwitch { pane: id, entry, shell },
                    pick("このTerminal Panelのシェルを切り替えますか？\n\n現在のセッションは終了します。", "Switch this Terminal Panel shell?\n\nIts current session will end.").into(),
                    &[pick("切り替える", "Switch"), cancel()], 0);
            } else {
                switch_confirmed(window, live, id, &entry, shell);
            }
            return;
        }
        11 => {
            let Some(path) = file_dialog::open_document(ime::window_handle(window)) else {
                return;
            };
            open_file(window, live, id, &path);
            return;
        }
        _ => return,
    }
    show(window, live, id);
}

fn import(window: &AppWindow, live: &Live, id: PaneId, action: i32) {
    let source = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .and_then(|t| t.terminal.clone());
    let Some(source) = source else {
        return;
    };
    if action == 7 {
        // Keep the existing Panel destination; file output has an independent cursor.
        if entries(live).iter().any(|p| {
            p.borrow()
                .capture
                .as_ref()
                .is_some_and(|s| Rc::ptr_eq(s, &source))
        }) {
            window.tell(
                pick(
                    "このTerminalは取り込み中です",
                    "This terminal is already being captured",
                )
                .into(),
            );
            return;
        }
        action_new(window, live, id);
        let Some(target) = current(live, id) else {
            return;
        };
        source.borrow_mut().drain();
        source.borrow_mut().start_capture();
        let mut target = target.borrow_mut();
        target.before_read_only = target.view.viewer;
        target.view.set_read_only(true, true);
        target.capture = Some(source);
        target.pending_tail = 0;
        target.log_name = Some(log_name());
    } else {
        let text = if action == 8 {
            source.borrow().screen().retained_text()
        } else {
            let selection = live
                .cache
                .borrow_mut()
                .pane(id)
                .terminal
                .as_ref()
                .and_then(|v| v.selection);
            selection
                .map(|s| terminal_selection_text(&source.borrow(), s))
                .unwrap_or_default()
        };
        if text.is_empty() {
            return;
        }
        if action == 8 {
            action_new(window, live, id);
        }
        let Some(target) = current(live, id) else {
            return;
        };
        let target = target.borrow();
        if target.view.viewer {
            window.tell(
                pick(
                    "ReadOnlyです。別のTABを選んでください",
                    "ReadOnly: choose another panel tab",
                )
                .into(),
            );
            return;
        }
        let mut buffer = target.document.text.borrow_mut();
        let mut at = if action == 8 {
            buffer.len()
        } else {
            id.screen(window).panel_caret.max(0) as usize
        }
        .min(buffer.len());
        while !buffer.is_char_boundary(at) {
            at -= 1;
        }
        buffer.insert_str(at, &text);
    }
    show(window, live, id);
}

pub(crate) fn log_name() -> String {
    let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "Terminal-{:04}{:02}{:02}-{:02}{:02}{:02}.txt",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    )
}

fn start_file(window: &AppWindow, live: &Live, id: PaneId) {
    let source = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .and_then(|t| t.terminal.clone());
    let Some(source) = source else {
        return;
    };
    if source.borrow().file_log_path().is_some() {
        window.tell(
            pick(
                "現在の取り込みを停止してから開始してください",
                "Stop the current capture before starting another",
            )
            .into(),
        );
        return;
    }
    let Some(chosen) = file_dialog::save_document_as(
        ime::window_handle(window),
        &log_name(),
        file_dialog::SaveFields::none(),
    ) else {
        return;
    };
    if logging_to(live, &chosen.path)
        || document_at(live, &chosen.path).is_some()
        || entries(live)
            .iter()
            .any(|e| e.borrow().document.file.borrow().path() == Some(chosen.path.as_path()))
    {
        window.tell(
            pick(
                "開いている文書とは別の保存先を選んでください",
                "Choose a file that is not open in the editor",
            )
            .into(),
        );
        return;
    }
    let current_source = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .and_then(|t| t.terminal.clone());
    if !current_source.is_some_and(|s| Rc::ptr_eq(&s, &source)) {
        return;
    }
    let file = match std::fs::File::create(&chosen.path) {
        Ok(file) => file,
        Err(error) => {
            window.tell(
                format!(
                    "{}: {error}",
                    pick("ログを開始できません", "Cannot start log")
                )
                .into(),
            );
            return;
        }
    };
    source.borrow_mut().start_file_log(chosen.path, file);
}

pub(crate) fn stop_for_close(window: &AppWindow, live: &Live) -> bool {
    for session in sessions(live) {
        stop_file(window, &session);
    }
    stop_entries_for_close(window, live, &entries(live))
}

pub(crate) fn stop_entries_for_close(
    window: &AppWindow,
    live: &Live,
    entries: &[Rc<RefCell<PanelDocument>>],
) -> bool {
    for entry in entries {
        entry.borrow_mut().stop();
        let save_tail = {
            let e = entry.borrow();
            e.save_tail_on_close && e.document.text.edited()
        };
        if save_tail && !save(window, live, entry, false) {
            return false;
        }
        entry.borrow_mut().save_tail_on_close = false;
    }
    true
}

pub(crate) fn save_for_close(
    window: &AppWindow,
    live: &Live,
    entry: &Rc<RefCell<PanelDocument>>,
) -> bool {
    if !save(window, live, entry, false) {
        return false;
    }
    entry.borrow_mut().save_tail_on_close = true;
    true
}

pub(crate) fn stop_all(live: &Live) {
    for session in sessions(live) {
        session.borrow_mut().stop_file_log();
    }
    for entry in entries(live) {
        entry.borrow_mut().stop();
    }
}

pub(crate) fn logging_to(live: &Live, path: &Path) -> bool {
    sessions(live).iter().any(|session| {
        session.borrow().file_log_path().is_some_and(|p| {
            p.to_string_lossy()
                .eq_ignore_ascii_case(&path.to_string_lossy())
                || std::fs::canonicalize(p)
                    .ok()
                    .zip(std::fs::canonicalize(path).ok())
                    .is_some_and(|(a, b)| a == b)
        })
    })
}

fn sessions(live: &Live) -> Vec<Rc<RefCell<TerminalSession>>> {
    live.tabs
        .borrow()
        .panes
        .iter()
        .flat_map(|p| &p.tabs)
        .flat_map(|t| t.terminal.iter().chain(t.below.shell.iter()).cloned())
        .collect()
}

pub(crate) fn stop_file(window: &AppWindow, session: &Rc<RefCell<TerminalSession>>) {
    let mut session = session.borrow_mut();
    session.stop_file_log();
    report_log_error(window, &mut session);
}

fn report_log_error(window: &AppWindow, session: &mut TerminalSession) {
    if let Some(error) = session.take_log_error() {
        window.tell(
            format!(
                "{}: {error}",
                pick(
                    "ファイル出力を停止しました。ログの保存に失敗しました",
                    "File capture stopped: could not save the log"
                )
            )
            .into(),
        );
    }
}

fn stop_source(window: &AppWindow, live: &Live, id: PaneId, file: bool) {
    let source = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .and_then(|t| t.terminal.clone());
    if let Some(source) = source {
        if file {
            stop_file(window, &source);
        } else {
            for entry in entries(live) {
                let matches = entry
                    .borrow()
                    .capture
                    .as_ref()
                    .is_some_and(|s| Rc::ptr_eq(s, &source));
                if matches {
                    entry.borrow_mut().stop();
                }
            }
        }
    }
    drain(window, live);
}

pub(crate) fn publish_source(window: &AppWindow, live: &Live, id: PaneId) {
    if let Some(tab) = live.tabs.borrow().of(id).current() {
        publish_source_for_tab(window, id, tab);
    }
}

pub(crate) fn publish_source_for_tab(window: &AppWindow, id: PaneId, tab: &PaneTab) {
    let file_path = tab
        .terminal
        .as_ref()
        .and_then(|s| {
            s.borrow()
                .file_log_path()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let panel = tab.terminal.as_ref().is_some_and(|s| {
        tab.below.entries.iter().any(|p| {
            p.borrow()
                .capture
                .as_ref()
                .is_some_and(|other| Rc::ptr_eq(s, other))
        })
    });
    id.update_screen(window, |screen| {
        screen.terminal_capturing = panel;
        screen.terminal_file_logging = !file_path.is_empty();
        screen.panel_log_destination = file_path.into();
    });
}

fn action_new(window: &AppWindow, live: &Live, id: PaneId) {
    action(window, live, id, 1, 0);
}

pub(crate) fn switch_confirmed(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    entry: &Rc<RefCell<PanelDocument>>,
    shell: TerminalShell,
) {
    if !entries(live).iter().any(|p| Rc::ptr_eq(p, entry)) {
        return;
    }
    if entry
        .borrow()
        .shell
        .as_ref()
        .is_some_and(|s| s.borrow().name() == shell.name)
    {
        return;
    }
    let height = live.cache.borrow_mut().pane(id).below_height;
    let Some(session) = start_shell(window, live, id, &shell, height) else {
        return;
    };
    entry.borrow_mut().shell = Some(Rc::new(RefCell::new(session)));
    for tab in live
        .tabs
        .borrow_mut()
        .panes
        .iter_mut()
        .flat_map(|p| &mut p.tabs)
    {
        if tab
            .below
            .entries
            .get(tab.below.active)
            .is_some_and(|p| Rc::ptr_eq(p, entry))
        {
            tab.below.shell = entry.borrow().shell.clone();
        }
    }
    // Only replace the cache if this entry is still visible.
    if current(live, id).is_some_and(|p| Rc::ptr_eq(&p, entry)) {
        show(window, live, id);
    }
}

pub(crate) fn open_file(window: &AppWindow, live: &Live, id: PaneId, path: &Path) {
    if document_at(live, path).is_some()
        || entries(live)
            .iter()
            .any(|e| e.borrow().document.file.borrow().path() == Some(path))
    {
        window.tell(
            pick(
                "このファイルは既に開いています",
                "This file is already open",
            )
            .into(),
        );
        return;
    }
    let (file, text) = match DocumentFile::open(path, MAX_DOCUMENT_CHARACTERS) {
        Ok(result) => result,
        Err(error) => {
            window.tell(format!("{}: {error}", pick("開けません", "Cannot open")).into());
            return;
        }
    };
    let doc = OpenDocument::new(file, text, slint::Weak::default());
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        let active = strip.active;
        let Some(tab) = strip.tabs.get_mut(active).filter(|t| t.terminal.is_some()) else {
            return;
        };
        tab.below
            .entries
            .push(Rc::new(RefCell::new(PanelDocument::new(window, doc, None))));
        tab.below.active = tab.below.entries.len() - 1;
    }
    show(window, live, id);
}

fn rename(window: &AppWindow, live: &Live, id: PaneId, index: usize, name: &str) {
    let entry = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .and_then(|t| t.below.entries.get(index).cloned());
    let Some(entry) = entry else {
        return;
    };
    if entry.borrow().capture.is_some() {
        window.tell(
            pick(
                "取り込みを停止してから名前を変更してください",
                "Stop capture before renaming",
            )
            .into(),
        );
        return;
    }
    let doc = entry.borrow().document.clone();
    let Some(path) = doc.file.borrow().path().map(Path::to_owned) else {
        window.tell(
            pick(
                "名前の変更: 先に保存してください",
                "Rename: save the document first",
            )
            .into(),
        );
        return;
    };
    let name = match file_tree::check_name(name) {
        Ok(name) => name,
        Err(e) => {
            window.tell(e.message().into());
            return;
        }
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let to = parent.join(name);
    if path == to {
        return;
    }
    if let Err(e) = move_entry(window, live, &path, &to) {
        window.tell(cannot_rename(&e).into());
        return;
    }
    doc.file.borrow_mut().follow_rename(to);
    publish_left(window, live);
    show(window, live, id);
}

pub(crate) fn entries(live: &Live) -> Vec<Rc<RefCell<PanelDocument>>> {
    live.tabs
        .borrow()
        .panes
        .iter()
        .flat_map(|p| &p.tabs)
        .flat_map(|t| t.below.entries.iter().cloned())
        .collect()
}

pub(crate) fn drain(window: &AppWindow, live: &Live) {
    let sessions: Vec<_> = live
        .tabs
        .borrow()
        .panes
        .iter()
        .flat_map(|p| &p.tabs)
        .flat_map(|t| t.terminal.iter().chain(t.below.shell.iter()).cloned())
        .collect();
    for session in sessions {
        let mut session = session.borrow_mut();
        session.set_history_limit(window.get_terminal_history_limit().max(0) as usize);
        session.drain();
        report_log_error(window, &mut session);
    }
    for entry in entries(live) {
        let mut entry = entry.borrow_mut();
        if let Some(shell) = &entry.shell {
            let mut shell = shell.borrow_mut();
            shell.set_history_limit(window.get_terminal_history_limit().max(0) as usize);
            shell.drain();
        }
        entry.history_limit = window.get_terminal_history_limit().max(0) as usize;
        entry.collect();
        if entry
            .capture
            .as_ref()
            .is_some_and(|s| s.borrow().finished())
        {
            entry.stop();
        }
    }
    let mut tabs = live.tabs.borrow_mut();
    for id in PaneId::all(window) {
        let strip = tabs.of_mut(id);
        for (index, tab) in strip.tabs.iter_mut().enumerate() {
            let Some(entry) = tab.below.entries.get(tab.below.active) else {
                continue;
            };
            let entry = entry.borrow();
            let text = entry.document.text.borrow();
            if tab.terminal.is_some() && tab.below.draft != *text {
                tab.below.draft = text.clone();
                if index == strip.active {
                    show_draft(window, id, &text);
                }
            }
            drop(text);
            if index == strip.active {
                drop(entry);
                publish(window, id, &tab.below);
            }
        }
    }
    drop(tabs);
    for id in PaneId::all(window) {
        publish_source(window, live, id);
    }
}

pub(crate) fn save(
    window: &AppWindow,
    live: &Live,
    entry: &Rc<RefCell<PanelDocument>>,
    ask: bool,
) -> bool {
    entry.borrow_mut().collect();
    let doc = entry.borrow().document.clone();
    let path = if ask {
        None
    } else {
        doc.file.borrow().path().map(Path::to_owned)
    };
    let target = match path {
        Some(path) => path,
        None => {
            let name = entry
                .borrow()
                .log_name
                .clone()
                .unwrap_or_else(|| doc.file.borrow().title());
            let owner = ime::window_handle(window);
            let Some(chosen) =
                file_dialog::save_document_as(owner, &name, file_dialog::SaveFields::none())
            else {
                return false;
            };
            chosen.path
        }
    };
    // Use the same writer and conflict detection as ordinary documents.
    if doc.file.borrow().path() == Some(target.as_path())
        && doc.file.borrow().external_change() != ExternalChange::None
    {
        window.tell(
            pick(
                "保存先が外部で変わっています。別名で保存してください",
                "The file changed outside; use Save As",
            )
            .into(),
        );
        return false;
    }
    let conflict = document_at(live, &target).is_some_and(|other| !Rc::ptr_eq(&other, &doc))
        || live
            .tabs
            .borrow()
            .panes
            .iter()
            .flat_map(|p| &p.tabs)
            .flat_map(|t| &t.below.entries)
            .any(|e| {
                let e = e.borrow();
                !Rc::ptr_eq(&e.document, &doc)
                    && e.document.file.borrow().path() == Some(target.as_path())
            });
    if conflict {
        window.tell(
            pick(
                "別のTABで開いている保存先です。別名で保存してください",
                "Another tab has this file open; use a different name",
            )
            .into(),
        );
        return false;
    }
    saving::write_document_to(window, live, &doc, target)
}

fn request_close(window: &AppWindow, live: &Live, id: PaneId) {
    let Some(entry) = current(live, id) else {
        return;
    };
    let held = entry.borrow();
    if held.document.text.edited()
        || held.capture.is_some()
        || (window.get_terminal_confirm_close()
            && held.shell.as_ref().is_some_and(|s| !s.borrow().finished()))
    {
        let save = held.document.text.edited() || held.capture.is_some();
        drop(held);
        let choices = if save {
            vec![
                pick("保存して閉じる", "Save and Close"),
                pick("保存せず閉じる", "Close Without Saving"),
                cancel(),
            ]
        } else {
            vec![pick("閉じる", "Close"), cancel()]
        };
        ask_question(window, live, Question::PanelClose { pane: id, entry, save },
            pick("PanelのTABを閉じますか？\n\nログ取り込みとシェルは閉じると停止します。キャンセルでは継続します。", "Close this panel tab?\n\nClosing stops its log capture and shell. Cancel keeps them running.").into(),
            &choices, if save { 1 } else { 0 });
    } else {
        drop(held);
        close(window, live, id, &entry);
    }
}

pub(crate) fn close(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    entry: &Rc<RefCell<PanelDocument>>,
) {
    if !stop_entries_for_close(window, live, std::slice::from_ref(entry)) {
        return;
    }
    let id = live
        .tabs
        .borrow()
        .panes
        .iter()
        .enumerate()
        .find_map(|(i, strip)| {
            strip
                .tabs
                .iter()
                .any(|t| t.below.entries.iter().any(|p| Rc::ptr_eq(p, entry)))
                .then(|| PaneId::from_index(i as i32))
        })
        .unwrap_or(id);
    let mut visible = false;
    {
        let mut tabs = live.tabs.borrow_mut();
        let strip = tabs.of_mut(id);
        for (index, tab) in strip.tabs.iter_mut().enumerate() {
            if !tab.below.entries.iter().any(|p| Rc::ptr_eq(p, entry)) {
                continue;
            }
            tab.below.entries.retain(|p| !Rc::ptr_eq(p, entry));
            tab.below.active = tab
                .below
                .active
                .min(tab.below.entries.len().saturating_sub(1));
            if let Some(next) = tab.below.entries.get(tab.below.active) {
                let next = next.borrow();
                tab.below.draft = next.document.text.borrow().clone();
                tab.below.shell = next.shell.clone();
            } else {
                tab.below.draft.clear();
                tab.below.shell = None;
                tab.below.open = false;
            }
            visible = index == strip.active;
        }
    }
    if visible {
        let below = live.tabs.borrow().of(id).current().map(|t| t.below.clone());
        if let Some(below) = below {
            if below.entries.is_empty() {
                let mut cache = live.cache.borrow_mut();
                let pane = cache.pane(id);
                pane.below_open = false;
                pane.below = None;
                id.set_below(window, 0, pane.below_height);
                publish(window, id, &below);
            } else {
                show(window, live, id);
            }
        }
    }
}

pub(crate) fn edited(window: &AppWindow, live: &Live, id: PaneId) {
    ensure(window, live, id);
    store_below_on_tab(window, live, id);
    if let Some(tab) = live.tabs.borrow().of(id).current() {
        publish(window, id, &tab.below);
    }
}

pub(crate) fn install(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    let scroll_live = live.clone();
    window.on_panel_scrolled(move |pane, position, end| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let id = PaneId::from_index(pane);
        if let Some(entry) = current(&scroll_live, id) {
            let follow = entry.borrow_mut().view.follow_at(position, end, false);
            id.update_screen(&window, |screen| screen.panel_follow = follow);
        }
    });
    let weak = window.as_weak();
    let rename_live = live.clone();
    window.on_panel_renamed(move |pane, index, name| {
        if let Some(window) = weak.upgrade() {
            rename(
                &window,
                &rename_live,
                PaneId::from_index(pane),
                index.max(0) as usize,
                &name,
            );
        }
    });
    let weak = window.as_weak();
    let live = live.clone();
    window.on_panel_action(move |pane, what, index| {
        if let Some(window) = weak.upgrade() {
            action(&window, &live, PaneId::from_index(pane), what, index);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panel_log_keeps_only_newest_lines() {
        for (source, limit, expected) in [
            ("一\n二\n三\n", 2, "二\n三\n"),
            ("一\n二\n途中", 2, "二\n途中"),
            ("一\n", 1, "一\n"),
            ("途中", 1, "途中"),
            ("途中", 0, ""),
            ("", 0, ""),
        ] {
            let mut text = source.to_owned();
            trim_log(&mut text, limit);
            assert_eq!(text, expected);
        }
    }
}
