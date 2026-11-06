//! Terminal confirmations carry their destination across modal dialogs.
use super::*;

#[derive(Clone)]
pub(crate) struct Input {
    session: Rc<RefCell<TerminalSession>>,
    text: String,
    draft: Option<Rc<RefCell<terminal_panels::PanelDocument>>>,
}
impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalInput").finish_non_exhaustive()
    }
}

pub(crate) fn input(
    window: &AppWindow,
    live: &Live,
    session: Rc<RefCell<TerminalSession>>,
    text: &str,
    draft: Option<Rc<RefCell<terminal_panels::PanelDocument>>>,
) {
    if text.is_empty() {
        return;
    }
    if session.borrow().finished() {
        tell_send_busy(window);
        return;
    }
    let input = Input {
        session,
        text: text.to_owned(),
        draft,
    };
    if window.get_terminal_confirm_paste() && text.contains(['\n', '\r']) {
        if live.pending.borrow().is_some() {
            return;
        }
        let preview = format!(
            "{} ({} {})\n\n{}{}",
            pick(
                "改行を含む内容をTerminalへ送りますか？ コマンドが実行される場合があります。",
                "Send text containing line breaks? This can execute commands."
            ),
            input
                .text
                .replace("\r\n", "\n")
                .replace('\r', "\n")
                .split('\n')
                .count(),
            pick("行", "lines"),
            input.text.chars().take(2000).collect::<String>(),
            if input.text.chars().count() > 2000 {
                "\n…"
            } else {
                ""
            }
        );
        ask_question(
            window,
            live,
            Question::TerminalInput(input),
            preview,
            &[pick("送信", "Send"), cancel()],
            0,
        );
    } else {
        send(window, live, input);
    }
}

pub(crate) fn send(window: &AppWindow, live: &Live, input: Input) {
    if input.session.borrow().finished() {
        tell_send_busy(window);
        return;
    }
    if let Some(entry) = input.draft {
        let entry = entry.borrow();
        if entry.view.borrow().viewer {
            return;
        }
        if !input.session.borrow_mut().send_draft(&input.text) {
            tell_send_busy(window);
            return;
        }
        // Do not delete edits made while confirmation was showing.
        if *entry.document.text.borrow() == input.text {
            entry.document.text.borrow_mut().clear();
        }
    } else if !input.session.borrow_mut().paste(&input.text) {
        tell_send_busy(window);
        return;
    }
    terminal_panels::drain(window, live);
    refresh_terminal_panes(window, live);
}

pub(crate) fn tell_send_busy(window: &AppWindow) {
    window.tell(
        crate::pick(
            "Terminal に送信できません。シェルの終了、入力の詰まり、または送信サイズの上限を確認してください",
            "Cannot send to Terminal. The shell may have stopped, input may be backed up, or the text may be too large",
        )
        .into(),
    );
}

pub(crate) fn needs_close(window: &AppWindow, tab: &PaneTab) -> bool {
    let shell = |s: &Rc<RefCell<TerminalSession>>| {
        window.get_terminal_confirm_close() && s.borrow_mut().needs_stop_confirmation()
    };
    tab.terminal
        .as_ref()
        .is_some_and(|s| shell(s) || s.borrow().file_log_path().is_some())
        || tab.below.shell.as_ref().is_some_and(shell)
        || tab.below.entries.iter().any(|p| {
            let p = p.borrow();
            panel_needs_save(&p) || p.capture.is_some() || p.shell.as_ref().is_some_and(shell)
        })
}

pub(crate) fn panel_needs_save(panel: &terminal_panels::PanelDocument) -> bool {
    let text = &panel.document.text;
    text.edited() && text.differs_from_saved()
}

pub(crate) fn ask_window_close(window: &AppWindow, live: &Live) -> bool {
    if !live
        .tabs
        .borrow()
        .panes
        .iter()
        .flat_map(|p| &p.tabs)
        .any(|t| needs_close(window, t))
    {
        return false;
    }
    let unsaved_panels = terminal_panels::entries(live)
        .iter()
        .any(|e| panel_needs_save(&e.borrow()));
    let unsaved_documents = open_documents(live).iter().any(|d| d.text.edited());
    let title = pick(
        "Terminalを停止して終了しますか？",
        "Stop terminals and close?",
    );
    let stop = pick(
        "実行中のシェルとログ取り込みは停止します。",
        "Running shells and log capture will stop.",
    );
    if unsaved_panels || unsaved_documents {
        let unsaved = if unsaved_panels {
            pick(
                "未保存のPanelがあります。",
                "Some panels have unsaved changes.",
            )
        } else {
            pick(
                "未保存の文書があります。",
                "Some documents have unsaved changes.",
            )
        };
        ask_question(
            window,
            live,
            Question::CloseWindow,
            format!("{title}\n\n{stop} {unsaved}"),
            &[
                pick("すべて保存して終了", "Save All and Close"),
                pick("保存せず終了", "Close Without Saving"),
                cancel(),
            ],
            1,
        );
    } else {
        ask_question(
            window,
            live,
            Question::StopTerminalsAndClose,
            format!("{title}\n\n{stop}"),
            &[pick("終了", "Close"), cancel()],
            0,
        );
    }
    true
}

pub(crate) fn ask_close(window: &AppWindow, live: &Live, identity: Rc<()>) {
    let (capturing, unsaved_panels) = live
        .tabs
        .borrow()
        .panes
        .iter()
        .flat_map(|p| &p.tabs)
        .find(|t| Rc::ptr_eq(&t.identity, &identity))
        .map(|t| {
            let capturing = t
                .terminal
                .as_ref()
                .is_some_and(|s| s.borrow().file_log_path().is_some())
                || t.below.entries.iter().any(|e| e.borrow().capture.is_some());
            let unsaved_panels = t
                .below
                .entries
                .iter()
                .any(|e| panel_needs_save(&e.borrow()));
            (capturing, unsaved_panels)
        })
        .unwrap_or_default();
    if !unsaved_panels {
        let message = if capturing {
            pick(
                "このTABを閉じますか？\n\nこのTABのシェルとログ取り込みが終了します。",
                "Close this tab?\n\nThis tab's shells and log capture will stop.",
            )
        } else {
            pick(
                "このTABを閉じますか？\n\nこのTABのシェルが終了します。",
                "Close this tab?\n\nThis tab's shells will stop.",
            )
        };
        ask_question(
            window,
            live,
            Question::TerminalClose(identity, false),
            message.into(),
            &[pick("閉じる", "Close"), cancel()],
            0,
        );
        return;
    }
    let message = if capturing {
        pick(
            "このTABと付属のPanelを閉じますか？\n\nこのTABのシェルとログ取り込みが終了します。別のTABの取り込みは継続します。Panelの未保存内容は保存してください。",
            "Close this tab and its panels?\n\nThis tab's shells and capture will stop. Other tabs keep capturing. Save any unsaved panel contents.",
        )
    } else {
        pick(
            "このTABと付属のPanelを閉じますか？\n\nこのTABのシェルが終了します。Panelの未保存内容は保存してください。",
            "Close this tab and its panels?\n\nThis tab's shells will stop. Save any unsaved panel contents.",
        )
    };
    ask_question(
        window,
        live,
        Question::TerminalClose(identity, true),
        message.into(),
        &[
            pick("Panelを保存して続行", "Save Panels and Continue"),
            pick("保存せず続行", "Continue Without Saving"),
            cancel(),
        ],
        1,
    );
}

fn tab_at_identity(live: &Live, identity: &Rc<()>) -> Option<(PaneId, usize)> {
    live.tabs
        .borrow()
        .panes
        .iter()
        .enumerate()
        .find_map(|(pane, strip)| {
            strip
                .tabs
                .iter()
                .position(|t| Rc::ptr_eq(&t.identity, identity))
                .map(|index| (PaneId::from_index(pane as i32), index))
        })
}

/// Close on the next tick, after the key handler has finished with the tab row.
pub(crate) fn request_close(window: &AppWindow, live: &Live, id: PaneId, spot: TerminalSpot) {
    let identity = match spot {
        TerminalSpot::Front => {
            let tabs = live.tabs.borrow();
            let Some(tab) = tabs.of(id).current().filter(|tab| tab.terminal.is_some()) else {
                return;
            };
            Some(tab.identity.clone())
        }
        TerminalSpot::Below => None,
    };
    let weak = window.as_weak();
    let live = live.clone();
    Timer::single_shot(Duration::ZERO, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if let Some(identity) = identity {
            if let Some((pane, index)) = tab_at_identity(&live, &identity) {
                close_tab(&window, &live, pane, index);
            }
        } else {
            terminal_panels::action(&window, &live, id, 2, 0);
        }
    });
}

pub(crate) fn close(window: &AppWindow, live: &Live, identity: Rc<()>, save: bool) {
    let Some((pane, index)) = tab_at_identity(live, &identity) else {
        return;
    };
    if save {
        let entries = live.tabs.borrow().of(pane).tabs[index]
            .below
            .entries
            .clone();
        for entry in &entries {
            let dirty = panel_needs_save(&entry.borrow());
            if dirty && !terminal_panels::save_for_close(window, live, entry) {
                cancel_close_run(live);
                return;
            }
        }
    }
    close_tab_content(window, live, pane, index);
}

pub(crate) fn action(window: &AppWindow, live: &Live, id: PaneId, spot: TerminalSpot, action: i32) {
    refresh_terminal_panes(window, live);
    let session = live
        .cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|s| s.session.clone());
    let Some(session) = session else {
        return;
    };
    match action {
        0 => {
            let target = if spot == TerminalSpot::Front { 0 } else { 1 };
            let screen = id.screen(window);
            if screen.terminal_search_open && screen.terminal_search_spot == target {
                self::action(window, live, id, spot, 3);
                return;
            }
            id.update_screen(window, |s| {
                s.terminal_search_open = true;
                s.terminal_search_spot = target;
            });
        }
        1 | 2 => {
            let screen = id.screen(window);
            let session = session.borrow();
            let found = session
                .screen()
                .search(screen.terminal_query.as_str(), screen.terminal_match_case);
            let mut cache = live.cache.borrow_mut();
            let Some(view) = cache.pane(id).shell(spot) else {
                return;
            };
            let current = view.selection.and_then(|selected| {
                found
                    .iter()
                    .position(|(a, b)| *a == selected.anchor && *b == selected.head)
            });
            let index = if found.is_empty() {
                0
            } else if action == 2 {
                current.map_or(found.len() - 1, |i| (i + found.len() - 1) % found.len())
            } else {
                current.map_or(0, |i| (i + 1) % found.len())
            };
            if let Some((anchor, head)) = found.get(index) {
                view.selection = Some(TerminalSelection {
                    anchor: *anchor,
                    head: *head,
                });
                view.looking = session.screen().scrollback().len().saturating_sub(anchor.0);
            }
            id.update_screen(window, |s| {
                s.terminal_search_status = if found.is_empty() {
                    "0 / 0".into()
                } else {
                    format!("{} / {}", index + 1, found.len()).into()
                }
            });
        }
        3 => {
            if let Some(view) = live.cache.borrow_mut().pane(id).shell(spot) {
                view.selection = None;
                view.looking = 0;
            }
            id.update_screen(window, |s| {
                s.terminal_search_open = false;
                s.terminal_query = "".into();
                s.terminal_search_status = "".into();
                if spot == TerminalSpot::Below {
                    s.below_focus_generation += 1;
                }
            });
            if spot == TerminalSpot::Front {
                restore_editor_focus(window);
            }
        }
        4 => {
            if let Some(view) = live.cache.borrow_mut().pane(id).shell(spot) {
                view.looking = 0;
            }
        }
        5 => session.borrow_mut().clear_screen(),
        6 => session.borrow_mut().clear_history(),
        7 => {
            let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
            let name = format!(
                "Terminal-{:04}{:02}{:02}-{:02}{:02}{:02}.txt",
                now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
            );
            let Some(chosen) = file_dialog::save_document_as(
                ime::window_handle(window),
                &name,
                file_dialog::SaveFields::none(),
            ) else {
                return;
            };
            if terminal_panels::logging_to(live, &chosen.path)
                || document_at(live, &chosen.path).is_some()
                || terminal_panels::entries(live).iter().any(|e| {
                    e.borrow().document.file.borrow().path() == Some(chosen.path.as_path())
                })
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
            session.borrow_mut().drain();
            let text = session.borrow().screen().retained_text();
            if let Err(error) = file_io::save(&chosen.path, &text, file_io::TextForm::default()) {
                window.tell(
                    format!(
                        "{}: {error}",
                        pick("ログを保存できません", "Cannot save log")
                    )
                    .into(),
                );
            }
        }
        8 | 9 => {
            let amount = session.borrow().screen().rows() as f32
                * 18.0
                * if action == 8 { 1.0 } else { -1.0 };
            scroll_terminal(window, live, id, spot, amount);
        }
        // RFN01-96（書き手と合意 2026-11-04）: Ctrl+↑／Ctrl+↓ — the command
        // before or after **is selected**, so something always shows where it
        // is (scrolling alone did nothing while every prompt was on screen),
        // and it is scrolled to the top only when it is above the view. Past
        // the last, the selection goes and the view is the latest again.
        20 | 21 => {
            let session = session.borrow();
            let screen = session.screen();
            let prompts = screen.prompt_rows();
            let history = screen.scrollback().len();
            let rows = screen.rows();
            let columns = screen.columns();
            let waiting = screen.cursor_line();
            if let Some(view) = live.cache.borrow_mut().pane(id).shell(spot) {
                // RFN01-96（書き手の求め 2026-11-04）: **the command, not the
                // prompt in front of it**, so Ctrl+Shift+C copies what was
                // typed. A prompt where nothing was typed is selected whole, so
                // where it is still shows.
                let span = |row: usize| {
                    let end = (screen.logical_end(row), columns);
                    let start = screen
                        .command_column(row)
                        .map(|column| (row, column))
                        .filter(|start| terminal_selection_text_at(&session, *start, end))
                        .unwrap_or((row, 0));
                    TerminalSelection {
                        anchor: start,
                        head: end,
                    }
                };
                // The command selected now, if the selection is one of these.
                let marked = view
                    .selection
                    .filter(|s| prompts.contains(&s.anchor.0) && *s == span(s.anchor.0))
                    .map(|s| s.anchor.0);
                let target = if action == 20 {
                    let from = marked.unwrap_or(waiting);
                    prompts.iter().rev().find(|row| **row < from).copied()
                } else {
                    marked.and_then(|from| prompts.iter().find(|row| **row > from).copied())
                };
                match target {
                    Some(row) if row < waiting => {
                        view.selection = Some(span(row));
                        let top = history - view.looking.min(history);
                        if row < top || row >= top + rows {
                            view.looking = history.saturating_sub(row);
                        }
                    }
                    _ if action == 21 => {
                        view.selection = None;
                        view.looking = 0;
                    }
                    _ => {}
                }
            }
        }
        // RFN01-102: Ctrl+Shift+W — the tab the keyboard is in closes, as its ×
        // would: the shell's own tab, or the strip's Terminal Panel tab. **On
        // the next tick**, like every tab command: the strip is republished.
        23 => {
            request_close(window, live, id, spot);
            return;
        }
        // RFN01-96: Copy Last Output.
        22 => {
            session.borrow_mut().drain();
            let text = session.borrow().screen().last_output();
            match text {
                Some(text) => {
                    clipboard::put_text(ime::window_handle(window), &text);
                }
                None => window.tell(
                    pick(
                        "直前のコマンドの出力が見つかりません",
                        "No output from a last command",
                    )
                    .into(),
                ),
            }
        }
        _ => {}
    }
    refresh_terminal(window, &live.cache, id, spot);
}

/// A pointer event over a shell (全画面TUIの互換性、2026-09-22). `true` when
/// the program took it, and the pane then starts no selection of its own.
///
/// `kind` is 0 press, 1 release, 2 move, 3 wheel; `button` is 0 left,
/// 1 middle, 2 right, 3 none. **Not while the history is being looked at**:
/// the rows on the pane are then not the program's screen.
#[allow(clippy::too_many_arguments)]
fn pointer(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    spot: TerminalSpot,
    (kind, button): (i32, i32),
    (x, y, delta): (f32, f32, f32),
    modifiers: TerminalModifiers,
) -> bool {
    use crate::terminal::{MouseAction, MouseButton};
    let Some((session, looking)) = live
        .cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|shell| (shell.session.clone(), shell.looking))
    else {
        return false;
    };
    if looking > 0 {
        return false;
    }
    let look = terminal_appearance::look(window, id, spot);
    let Ok(cell) = cells::terminal_cell_size(&look) else {
        return false;
    };
    let mut session = session.borrow_mut();
    if session.finished() {
        return false;
    }
    let screen = session.screen();
    let row = ((y.max(0.0) / cell.line) as usize).min(screen.rows().saturating_sub(1));
    let column = ((x.max(0.0) / cell.advance) as usize).min(screen.columns().saturating_sub(1));
    if kind == 3 {
        let lines = ((delta.abs() / cell.line).round() as usize).max(1);
        return session.send_wheel(delta > 0.0, lines, row, column, modifiers);
    }
    let action = match kind {
        0 => MouseAction::Press,
        1 => MouseAction::Release,
        _ => MouseAction::Move,
    };
    let button = match button {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => MouseButton::None,
    };
    let taken = session.send_mouse(action, button, row, column, modifiers);
    if taken && action == MouseAction::Press {
        live.cache.borrow_mut().log_diag(
            "terminal",
            &format!(
                "mouse pane={} spot={spot:?} button={button:?} row={row} column={column}",
                id.log_name()
            ),
        );
    }
    taken
}

thread_local! {
    /// RFN01-95: the pane whose strip along the foot has the keyboard, if one
    /// has. **Told by the strip's own field**, which is a separate element
    /// and so does say when it takes and loses the keyboard.
    static BELOW_KEYS: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

/// RFN01-95: tell every shell whether the keyboard is in it, and answer the
/// bell of the one where it is. True when a mark went away.
///
/// **Worked out from where things are, not from focus signals.** Switching
/// tabs inside one pane keeps the same field focused, so no signal says the
/// keyboard moved from a document to the shell that took its place (measured
/// in the app, 2026-11-04: the bell stayed after the tab was opened). The
/// keyboard is in the shell in front of the pane that has it — the strip's
/// when that has it.
pub(crate) fn settle_keyboard(window: &AppWindow, live: &Live) -> bool {
    let mut pane = focused_pane(window);
    if pane.is_panel() {
        pane = PaneId::from_index(pane.screen(window).panel_owner);
    }
    // **Only a strip that is open can have the keyboard** — a closed one's
    // field said nothing as it went (書き手の報告 2026-11-04: the bell stayed).
    let below =
        BELOW_KEYS.get() == Some(pane.index()) && live.cache.borrow_mut().pane(pane).below_open;
    let keyed = live.tabs.borrow().of(pane).current().and_then(|tab| {
        if below {
            tab.below
                .entries
                .get(tab.below.active)
                .and_then(|entry| entry.borrow().shell.clone())
                .or_else(|| tab.below.shell.clone())
        } else {
            tab.terminal.clone()
        }
    });
    let mut answered = false;
    let tabs = live.tabs.borrow();
    let shells = tabs
        .panes
        .iter()
        .flat_map(|strip| &strip.tabs)
        .flat_map(|tab| {
            tab.terminal
                .iter()
                .cloned()
                .chain(tab.below.shell.iter().cloned())
                .chain(
                    tab.below
                        .entries
                        .iter()
                        .filter_map(|entry| entry.borrow().shell.clone()),
                )
                .collect::<Vec<_>>()
        });
    for shell in shells {
        let here = keyed
            .as_ref()
            .is_some_and(|keyed| Rc::ptr_eq(keyed, &shell));
        answered |= shell.borrow_mut().set_keyboard(here);
    }
    answered
}

/// RFN01-96: whether anything but blanks lies from `start` to `end` — a
/// command was typed after this prompt.
fn terminal_selection_text_at(
    session: &TerminalSession,
    start: (usize, usize),
    end: (usize, usize),
) -> bool {
    let picked = TerminalSelection {
        anchor: start,
        head: end,
    };
    !crate::terminal_selection_text(session, picked)
        .trim()
        .is_empty()
}

/// The strip along `pane`'s foot was closed: its field no longer has the
/// keyboard, whatever it last said.
pub(crate) fn below_closed(pane: PaneId) {
    if BELOW_KEYS.get() == Some(pane.index()) {
        BELOW_KEYS.set(None);
    }
}

enum Opens {
    Url(String),
    File {
        lookup: FileLookup,
        line: Option<usize>,
        column: Option<usize>,
    },
}

/// RFN01-95: the file a name printed by a shell stands for — **only one that
/// exists and that the editor opens as text.** A relative name is read from
/// `folder`, the folder the shell started in; WSL's `/mnt/d/…` is `D:\…`;
/// another Linux path, or `~`, has no Windows name here.
pub(crate) fn resolve_path(text: &str, folder: Option<&Path>) -> Option<PathBuf> {
    let path = if let Some(path) = terminal_shells::wsl_mount(text) {
        path
    } else if text.starts_with('/') || text.starts_with('~') {
        return None;
    } else {
        let path = Path::new(text);
        if path.has_root() || path.is_absolute() {
            path.to_path_buf()
        } else {
            folder?.join(path)
        }
    };
    (path.is_file() && opens_as_text(&path)).then_some(path)
}

/// RFN01-95（案A）: a prompt's folder as a Windows folder. `/mnt/d/…` and
/// `D:\…` are; `~` and other Linux folders are not.
fn prompt_folder(written: &str) -> Option<PathBuf> {
    if let Some(path) = terminal_shells::wsl_mount(written) {
        return Some(path);
    }
    if is_linux_path(written) {
        return None;
    }
    let path = Path::new(written);
    path.is_absolute().then(|| path.to_path_buf())
}

/// RFN01-100: a path only WSL can name — `~`, or Linux's own folders (not a
/// mounted Windows drive).
fn is_linux_path(text: &str) -> bool {
    text == "~"
        || text.starts_with("~/")
        || (text.starts_with('/') && terminal_shells::wsl_mount(text).is_none())
}

struct FileLookup {
    text: String,
    prompt: Option<String>,
    folder: Option<PathBuf>,
    launch_directory: Option<String>,
    /// Some(None) is a WSL profile using the default distribution.
    distribution: Option<Option<String>>,
}

impl FileLookup {
    fn resolve(
        &self,
        resolve_wsl: impl Fn(Option<&str>, &str) -> Option<PathBuf>,
    ) -> Option<PathBuf> {
        let posix = |path: &str| path.starts_with('/') || path == "~" || path.starts_with("~/");
        let text = self.text.as_str();
        let wsl_file = |linux: &str| {
            resolve_wsl(self.distribution.as_ref()?.as_deref(), linux)
                .filter(|path| path.is_file() && opens_as_text(path))
        };
        if self.distribution.is_some() && posix(text) {
            return wsl_file(text);
        }
        if Path::new(text).has_root() || posix(text) {
            return resolve_path(text, None);
        }
        // The nearest prompt is the best evidence of where the command ran.
        // A relative POSIX path must be resolved as a whole to follow links
        // in both the directory and the final filename.
        for folder in self.prompt.iter().chain(self.launch_directory.iter()) {
            let found = if self.distribution.is_some() && posix(folder) {
                wsl_file(&format!("{}/{text}", folder.trim_end_matches('/')))
            } else {
                prompt_folder(folder).and_then(|folder| resolve_path(text, Some(&folder)))
            };
            if found.is_some() {
                return found;
            }
        }
        resolve_path(text, self.folder.as_deref())
    }
}

/// Pictures and the usual binaries are not opened from a shell (RFN01-95: 「エディタで
/// 開けないファイルは何もしない」).
fn opens_as_text(path: &Path) -> bool {
    const BINARY: [&str; 22] = [
        "exe", "dll", "so", "o", "obj", "lib", "pdb", "bin", "zip", "7z", "gz", "tar", "rar",
        "pdf", "docx", "xlsx", "pptx", "mp3", "mp4", "wav", "class", "jar",
    ];
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    !workspace_index::is_image(path) && !BINARY.contains(&extension.as_str())
}

/// RFN01-95: the link under a point of a pane's shell, and the underline for it
/// in the pane's coordinates. `None` when there is nothing there to open.
fn link_under(
    window: &AppWindow,
    live: &Live,
    id: PaneId,
    spot: TerminalSpot,
    x: f32,
    y: f32,
) -> Option<(Opens, Vec<PreviewSelectionRect>, u64)> {
    let (session, looking) = live
        .cache
        .borrow_mut()
        .pane(id)
        .shell(spot)
        .map(|v| (v.session.clone(), v.looking))?;
    let look = terminal_appearance::look(window, id, spot);
    let cell = cells::terminal_cell_size(&look).ok()?;
    let session = session.borrow();
    let screen = session.screen();
    let top = screen.scrollback().len().saturating_sub(looking);
    let row = top + (y.max(0.0) / cell.line) as usize;
    let column = (x.max(0.0) / cell.advance) as usize;
    let link = screen.link_at(row, column)?;
    let opens = match link.target {
        terminal::LinkTarget::Url(url) => Opens::Url(url),
        terminal::LinkTarget::Path { text, line, column } => Opens::File {
            lookup: FileLookup {
                text,
                prompt: screen.prompt_folder_above(row),
                folder: session.folder().map(Path::to_path_buf),
                launch_directory: session.launch_directory().map(str::to_owned),
                distribution: session
                    .profile()
                    .and_then(|profile| terminal_shells::wsl_distribution(&profile.command)),
            },
            line,
            column,
        },
    };
    let rows = screen.rows();
    let underline = link
        .cells
        .iter()
        .filter(|(at, _, _)| *at >= top && *at < top + rows)
        .map(|&(at, from, to)| PreviewSelectionRect {
            x: from as f32 * cell.advance,
            y: (at - top + 1) as f32 * cell.line - 2.0,
            width: (to - from) as f32 * cell.advance,
            height: 1.0,
        })
        .collect();
    Some((opens, underline, session.number()))
}

/// RFN01-95: Ctrl held over a link underlines it, and says so (the pointer
/// becomes a hand). True when there is a link there.
fn hover(window: &AppWindow, live: &Live, id: PaneId, spot: TerminalSpot, x: f32, y: f32) -> bool {
    thread_local! {
        /// Some pane has an underline up. **Every movement of the pointer over
        /// a shell asks**, and without Ctrl and with nothing to take down
        /// there is nothing to do — not even reading the pane's row.
        static UNDERLINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    let control = input_platform::control_held();
    if !control && !UNDERLINED.get() {
        return false;
    }
    let underline = control
        .then(|| link_under(window, live, id, spot, x, y))
        .flatten()
        .and_then(|(opens, underline, _)| match opens {
            Opens::Url(_) => Some(underline),
            Opens::File { lookup, .. } => lookup
                .resolve(|distribution, linux| {
                    let weak = window.as_weak();
                    terminal_shells::cached_wsl_windows_path(distribution, linux, move || {
                        let _ = weak.upgrade_in_event_loop(|window| {
                            let links = window.global::<TerminalLinks>();
                            links.set_generation(links.get_generation() + 1);
                        });
                    })
                })
                .map(|_| underline),
        })
        .unwrap_or_default();
    let shown = !underline.is_empty();
    UNDERLINED.set(shown);
    let screen = id.screen(window);
    let current = match spot {
        TerminalSpot::Front => screen.terminal_link,
        TerminalSpot::Below => screen.below_link,
    };
    if !current.iter().eq(underline.iter().cloned()) {
        id.update_screen(window, |screen| {
            let model = ModelRc::new(VecModel::from(underline));
            match spot {
                TerminalSpot::Front => screen.terminal_link = model,
                TerminalSpot::Below => screen.below_link = model,
            }
        });
    }
    shown
}

/// RFN01-95: where a file named in a shell opens. **Beside the shell, when the
/// window is split**: in another pane showing a document (the first of them),
/// or else another pane; with one pane, a new tab in it.
fn pane_for_file(window: &AppWindow, live: &Live, from: PaneId) -> PaneId {
    let others: Vec<PaneId> = PaneId::all(window)
        .into_iter()
        .filter(|p| *p != from && !p.is_panel())
        .collect();
    let tabs = live.tabs.borrow();
    others
        .iter()
        .copied()
        .find(|p| {
            tabs.of(*p)
                .current()
                .is_some_and(|t| !t.stands_in() && !t.empty)
        })
        .or_else(|| others.first().copied())
        .unwrap_or(from)
}

/// RFN01-95: open `path` and put the caret on `line` (and `column`), both
/// counted from 1 the way compilers and agents print them.
fn open_file_at(
    window: &AppWindow,
    live: &Live,
    from: PaneId,
    path: &Path,
    line: Option<usize>,
    column: Option<usize>,
) {
    let target = pane_for_file(window, live, from);
    window.set_focused_pane(target.index());
    open_path_in_pane(window, live, target, path, Opening::Kept);
    let document = live.states.document(target);
    if document.file.borrow().path() != Some(path) {
        return;
    }
    let source = document.text.borrow().clone();
    let at = line.map_or(0, |line| byte_of(&source, line, column.unwrap_or(1)));
    show_source_range(window, live, target, &source, at, at);
    restore_editor_focus(window);
}

/// The byte where 1-based `line` and `column` (in characters) fall, held to
/// the end of that line and of the text.
pub(crate) fn byte_of(source: &str, line: usize, column: usize) -> usize {
    let mut start = 0;
    for _ in 1..line.max(1) {
        match source[start..].find('\n') {
            Some(at) => start += at + 1,
            None => return source.len(),
        }
    }
    let end = source[start..]
        .find('\n')
        .map_or(source.len(), |at| start + at);
    let line_text = source[start..end].trim_end_matches('\r');
    start
        + line_text
            .char_indices()
            .nth(column.max(1) - 1)
            .map_or(line_text.len(), |(at, _)| at)
}

pub(crate) fn install(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    let mouse_live = live.clone();
    window.on_terminal_mouse(
        move |pane, spot, kind, button, x, y, delta, shift, control, alt| {
            let Some(window) = weak.upgrade() else {
                return false;
            };
            let spot = if spot == 0 {
                TerminalSpot::Front
            } else {
                TerminalSpot::Below
            };
            pointer(
                &window,
                &mouse_live,
                PaneId::from_index(pane),
                spot,
                (kind, button),
                (x, y, delta),
                TerminalModifiers {
                    shift,
                    alt,
                    control,
                },
            )
        },
    );
    let focus_live = live.clone();
    let focus_window = window.as_weak();
    window.on_terminal_focus(move |pane, spot, focused| {
        let spot = if spot == 0 {
            TerminalSpot::Front
        } else {
            TerminalSpot::Below
        };
        let session = focus_live
            .cache
            .borrow_mut()
            .pane(PaneId::from_index(pane))
            .shell(spot)
            .map(|shell| shell.session.clone());
        if let Some(session) = session {
            let mut borrowed = session.borrow_mut();
            if !borrowed.finished() {
                borrowed.send_focus(focused);
            }
            drop(borrowed);
        }
        // RFN01-95: the keyboard coming to a shell answers its bell. Which
        // shell has it is worked out again rather than taken from this one
        // signal (`settle_keyboard`).
        if spot == TerminalSpot::Below {
            BELOW_KEYS.set(focused.then_some(pane));
        }
        if let Some(window) = focus_window.upgrade()
            && settle_keyboard(&window, &focus_live)
        {
            crate::publish_tabs(&window, &focus_live);
            crate::refresh_terminal_panes(&window, &focus_live);
        }
    });
    let weak = window.as_weak();
    let url_live = live.clone();
    window.on_terminal_url(move |pane, spot, x, y| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let id = PaneId::from_index(pane);
        let spot = if spot == 0 {
            TerminalSpot::Front
        } else {
            TerminalSpot::Below
        };
        let Some((opens, _, number)) = link_under(&window, &url_live, id, spot, x, y) else {
            return;
        };
        match opens {
            Opens::Url(url) => {
                use windows::Win32::UI::{
                    Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL,
                };
                use windows::core::{HSTRING, w};
                unsafe {
                    ShellExecuteW(
                        ime::window_handle(&window),
                        w!("open"),
                        &HSTRING::from(url),
                        None,
                        None,
                        SW_SHOWNORMAL,
                    );
                }
            }
            Opens::File {
                lookup,
                line,
                column,
            } if lookup.distribution.is_some() => {
                let weak = window.as_weak();
                let coordinate =
                    |at: Option<usize>| at.and_then(|at| i32::try_from(at).ok()).unwrap_or(0);
                if !terminal_shells::start_fresh_wsl_lookup(move || {
                    let path = lookup.resolve(terminal_shells::ask_wsl);
                    let _ = weak.upgrade_in_event_loop(move |window| {
                        window.invoke_terminal_wsl_open(
                            pane,
                            if spot == TerminalSpot::Front { 0 } else { 1 },
                            number.to_string().into(),
                            path.map_or_else(SharedString::default, |path| {
                                path.to_string_lossy().into_owned().into()
                            }),
                            coordinate(line),
                            coordinate(column),
                        );
                    });
                }) {
                    window.tell(
                        pick(
                            "WSL のパスを確認中です。もう一度お試しください",
                            "WSL path lookups are busy. Try again shortly",
                        )
                        .into(),
                    );
                }
            }
            Opens::File {
                lookup,
                line,
                column,
            } => {
                if let Some(path) = lookup.resolve(|_, _| None) {
                    open_file_at(&window, &url_live, id, &path, line, column);
                }
            }
        }
    });
    let weak = window.as_weak();
    let wsl_live = live.clone();
    window.on_terminal_wsl_open(move |pane, spot, number, path, line, column| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let id = PaneId::from_index(pane);
        let spot = if spot == 0 {
            TerminalSpot::Front
        } else {
            TerminalSpot::Below
        };
        let Ok(number) = number.parse::<u64>() else {
            return;
        };
        if !id.is_shown(&window)
            || !wsl_live
                .cache
                .borrow_mut()
                .pane(id)
                .shell(spot)
                .is_some_and(|shell| shell.session.borrow().number() == number)
        {
            return;
        }
        if path.is_empty() {
            window.tell(
                pick(
                    "WSL のリンク先を開けません",
                    "Could not open the WSL link target",
                )
                .into(),
            );
            return;
        }
        let coordinate = |at| (at > 0).then_some(at as usize);
        open_file_at(
            &window,
            &wsl_live,
            id,
            Path::new(path.as_str()),
            coordinate(line),
            coordinate(column),
        );
    });
    let weak = window.as_weak();
    let hover_live = live.clone();
    window.on_pane_terminal_link_hover(move |pane, spot, x, y| {
        weak.upgrade().is_some_and(|window| {
            let spot = if spot == 0 {
                TerminalSpot::Front
            } else {
                TerminalSpot::Below
            };
            hover(&window, &hover_live, PaneId::from_index(pane), spot, x, y)
        })
    });
    let weak = window.as_weak();
    let live = live.clone();
    window.on_terminal_action(move |pane, spot, what| {
        if let Some(window) = weak.upgrade() {
            action(
                &window,
                &live,
                PaneId::from_index(pane),
                if spot == 0 {
                    TerminalSpot::Front
                } else {
                    TerminalSpot::Below
                },
                what,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFN01-95: a name printed by a shell opens only when it is a text file
    /// that exists, read from the folder the shell started in.
    #[test]
    fn printed_names_resolve_to_files_that_exist() {
        let folder = std::env::temp_dir().join(format!("rfn-links-{}", std::process::id()));
        std::fs::create_dir_all(folder.join("src")).unwrap();
        std::fs::write(folder.join("第一章.md"), "一\n二\n").unwrap();
        std::fs::write(folder.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(folder.join("cover.png"), [0_u8; 4]).unwrap();
        let base = Some(folder.as_path());
        assert_eq!(
            resolve_path("第一章.md", base),
            Some(folder.join("第一章.md"))
        );
        assert_eq!(
            resolve_path("src/main.rs", base),
            Some(folder.join("src/main.rs"))
        );
        assert_eq!(resolve_path("ない.md", base), None, "not there");
        assert_eq!(resolve_path("cover.png", base), None, "a picture");
        assert_eq!(
            resolve_path("第一章.md", None),
            None,
            "no folder to read from"
        );
        assert_eq!(resolve_path("~/notes.md", base), None);
        assert_eq!(resolve_path("/home/me/notes.md", base), None);
        let absolute = folder.join("第一章.md");
        assert_eq!(
            resolve_path(&absolute.to_string_lossy(), None),
            Some(absolute)
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn wsl_mounts_and_start_folders_have_windows_names() {
        assert_eq!(
            terminal_shells::wsl_mount("/mnt/d/原稿/a.md"),
            Some(PathBuf::from("D:\\原稿\\a.md"))
        );
        assert_eq!(
            terminal_shells::wsl_mount("/mnt/c"),
            Some(PathBuf::from("C:\\"))
        );
        assert_eq!(terminal_shells::wsl_mount("/mnt/data/x"), None);
        assert_eq!(terminal_shells::windows_folder("~"), None);
        assert_eq!(terminal_shells::windows_folder("/home/me"), None);
        assert_eq!(
            terminal_shells::windows_folder("D:\\原稿"),
            Some(PathBuf::from("D:\\原稿"))
        );
        assert_eq!(
            terminal_shells::windows_folder(""),
            std::env::current_dir().ok()
        );
    }

    #[test]
    fn prompt_folders_have_windows_names() {
        assert_eq!(
            prompt_folder("/mnt/d/原稿/第一部"),
            Some(PathBuf::from("D:\\原稿\\第一部"))
        );
        assert_eq!(
            prompt_folder("D:\\work\\book"),
            Some(PathBuf::from("D:\\work\\book"))
        );
        assert_eq!(prompt_folder("~/notes"), None, "not a WSL shell");
        assert_eq!(prompt_folder("/home/ken"), None);
        assert!(is_linux_path("~") && is_linux_path("~/a") && is_linux_path("/home/ken"));
        assert!(!is_linux_path("/mnt/d/x") && !is_linux_path("a/b") && !is_linux_path("D:\\x"));
    }

    #[test]
    fn lines_and_columns_count_from_one() {
        let source = "一行目\r\n二行目です\nthird";
        assert_eq!(byte_of(source, 1, 1), 0);
        assert_eq!(byte_of(source, 2, 1), "一行目\r\n".len());
        assert_eq!(byte_of(source, 2, 3), "一行目\r\n二行".len());
        assert_eq!(
            byte_of(source, 2, 99),
            "一行目\r\n二行目です".len(),
            "held to the end of the line"
        );
        assert_eq!(byte_of(source, 9, 1), source.len(), "held to the end");
        assert_eq!(byte_of(source, 0, 0), 0);
    }
}
