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
    if text.is_empty() || session.borrow().finished() {
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
        return;
    }
    if let Some(entry) = input.draft {
        let entry = entry.borrow();
        if entry.view.borrow().viewer {
            return;
        }
        input
            .session
            .borrow_mut()
            .send(&terminal::encode_typing(&input.text));
        // Do not delete edits made while confirmation was showing.
        if *entry.document.text.borrow() == input.text {
            entry.document.text.borrow_mut().clear();
        }
    } else {
        input.session.borrow_mut().paste(&input.text);
    }
    terminal_panels::drain(window, live);
    refresh_terminal_panes(window, live);
}

pub(crate) fn needs_close(window: &AppWindow, tab: &PaneTab) -> bool {
    let shell = |s: &Rc<RefCell<TerminalSession>>| {
        // RFN01-97: a shell put back from the last run and never shown has
        // nothing running to lose.
        let s = s.borrow();
        window.get_terminal_confirm_close() && !s.finished() && !s.is_waiting()
    };
    tab.terminal
        .as_ref()
        .is_some_and(|s| shell(s) || s.borrow().file_log_path().is_some())
        || tab.below.shell.as_ref().is_some_and(shell)
        || tab.below.entries.iter().any(|p| {
            let p = p.borrow();
            p.document.text.edited() || p.capture.is_some() || p.shell.as_ref().is_some_and(shell)
        })
}

pub(crate) fn ask_close(window: &AppWindow, live: &Live, identity: Rc<()>) {
    let capturing = live
        .tabs
        .borrow()
        .panes
        .iter()
        .flat_map(|p| &p.tabs)
        .find(|t| Rc::ptr_eq(&t.identity, &identity))
        .is_some_and(|t| {
            t.terminal
                .as_ref()
                .is_some_and(|s| s.borrow().file_log_path().is_some())
                || t.below.entries.iter().any(|e| e.borrow().capture.is_some())
        });
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
        Question::TerminalClose(identity),
        message.into(),
        &[
            pick("Panelを保存して続行", "Save Panels and Continue"),
            pick("保存せず続行", "Continue Without Saving"),
            cancel(),
        ],
        1,
    );
}

pub(crate) fn close(window: &AppWindow, live: &Live, identity: Rc<()>, save: bool) {
    let target = live
        .tabs
        .borrow()
        .panes
        .iter()
        .enumerate()
        .find_map(|(pane, strip)| {
            strip
                .tabs
                .iter()
                .position(|t| Rc::ptr_eq(&t.identity, &identity))
                .map(|index| {
                    (
                        PaneId::from_index(pane as i32),
                        index,
                        strip.tabs[index].below.entries.clone(),
                    )
                })
        });
    let Some((pane, index, entries)) = target else {
        return;
    };
    if save {
        for entry in &entries {
            let dirty = entry.borrow().document.text.edited();
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
        // RFN01-96: Ctrl+↑／Ctrl+↓ — the prompt before or after the one at the
        // top of the view comes to the top. Past the last, the latest.
        20 | 21 => {
            let (prompts, history) = {
                let session = session.borrow();
                let screen = session.screen();
                (screen.prompt_rows(), screen.scrollback().len())
            };
            if let Some(view) = live.cache.borrow_mut().pane(id).shell(spot) {
                let top = history - view.looking.min(history);
                let target = if action == 20 {
                    prompts.iter().rev().find(|row| **row < top).copied()
                } else {
                    prompts.iter().find(|row| **row > top).copied()
                };
                match target {
                    Some(row) if row < history => view.looking = history - row,
                    _ if action == 21 => view.looking = 0,
                    _ => {}
                }
            }
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
    let below = BELOW_KEYS.get() == Some(pane.index());
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

/// RFN01-95: what a link in a shell opens, once it is known to be openable.
#[derive(Debug, PartialEq)]
pub(crate) enum Opens {
    Url(String),
    File {
        path: PathBuf,
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
fn prompt_folder(written: &str, wsl: Option<&WslAsk>) -> Option<PathBuf> {
    if let Some(path) = terminal_shells::wsl_mount(written) {
        return Some(path);
    }
    if is_linux_path(written) {
        // RFN01-100: `~/Dev/10_Editor` is wherever WSL says it is.
        return wsl?.windows(written);
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

/// RFN01-100: asking WSL from a shell that is WSL. **Without waiting from the
/// pointer** (the answer comes back on a thread and the window measures
/// again), waiting from a click.
struct WslAsk {
    distribution: Option<String>,
    wait: bool,
    window: slint::Weak<AppWindow>,
}

impl WslAsk {
    fn of(window: &AppWindow, session: &TerminalSession, wait: bool) -> Option<Self> {
        let distribution = terminal_shells::wsl_distribution(&session.profile()?.command)?;
        Some(Self {
            distribution,
            wait,
            window: window.as_weak(),
        })
    }

    fn windows(&self, linux: &str) -> Option<PathBuf> {
        let weak = self.window.clone();
        terminal_shells::wsl_windows_path(
            self.distribution.as_deref(),
            linux,
            self.wait,
            move || {
                // The pointer resting on the link is measured again.
                let _ = weak.upgrade_in_event_loop(|window| {
                    let links = window.global::<TerminalLinks>();
                    links.set_generation(links.get_generation() + 1);
                });
            },
        )
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
    wait: bool,
) -> Option<(Opens, Vec<PreviewSelectionRect>)> {
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
            // RFN01-95（案A、書き手と合意 2026-11-04）: **the folder the nearest
            // prompt names first** — after a `cd` it is where the shell, and an
            // agent started there, is; a name in both folders means the one
            // the prompt is in. Then the folder the shell started in.
            //
            // RFN01-100: a Linux path — `~/…`, `/home/…` — is asked of WSL,
            // which follows its links; only one that lands on a Windows drive
            // opens.
            path: {
                let wsl = WslAsk::of(window, &session, wait);
                if is_linux_path(&text) {
                    wsl.as_ref()
                        .and_then(|wsl| wsl.windows(&text))
                        .filter(|path| path.is_file() && opens_as_text(path))?
                } else {
                    screen
                        .prompt_folder_above(row)
                        .and_then(|folder| prompt_folder(&folder, wsl.as_ref()))
                        .and_then(|folder| resolve_path(&text, Some(&folder)))
                        .or_else(|| resolve_path(&text, session.folder()))?
                }
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
    Some((opens, underline))
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
        .then(|| link_under(window, live, id, spot, x, y, false))
        .flatten()
        .map(|(_, underline)| underline)
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
        match link_under(&window, &url_live, id, spot, x, y, true).map(|(opens, _)| opens) {
            Some(Opens::Url(url)) => {
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
            Some(Opens::File { path, line, column }) => {
                open_file_at(&window, &url_live, id, &path, line, column);
            }
            None => {}
        }
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
            prompt_folder("/mnt/d/原稿/第一部", None),
            Some(PathBuf::from("D:\\原稿\\第一部"))
        );
        assert_eq!(
            prompt_folder("D:\\work\\book", None),
            Some(PathBuf::from("D:\\work\\book"))
        );
        assert_eq!(prompt_folder("~/notes", None), None, "not a WSL shell");
        assert_eq!(prompt_folder("/home/ken", None), None);
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
