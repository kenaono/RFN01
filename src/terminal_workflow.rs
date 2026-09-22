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
        window.get_terminal_confirm_close() && !s.borrow().finished()
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
            let mut session = session.borrow_mut();
            if !session.finished() {
                session.send_focus(focused);
            }
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
        let Some((session, looking)) = url_live
            .cache
            .borrow_mut()
            .pane(id)
            .shell(spot)
            .map(|v| (v.session.clone(), v.looking))
        else {
            return;
        };
        let look = terminal_appearance::look(&window, id, spot);
        let Ok(cell) = cells::terminal_cell_size(&look) else {
            return;
        };
        let session = session.borrow();
        let top = session.screen().scrollback().len().saturating_sub(looking);
        let row = top + (y.max(0.0) / cell.line) as usize;
        let column = (x.max(0.0) / cell.advance) as usize;
        if let Some(url) = session.screen().url_at(row, column) {
            use windows::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
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
