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
        if entry.view.viewer {
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
    tab.terminal.as_ref().is_some_and(shell)
        || tab.below.shell.as_ref().is_some_and(shell)
        || tab.below.entries.iter().any(|p| {
            let p = p.borrow();
            p.document.text.edited() || p.capture.is_some() || p.shell.as_ref().is_some_and(shell)
        })
}

pub(crate) fn ask_close(window: &AppWindow, live: &Live, identity: Rc<()>) {
    ask_question(window, live, Question::TerminalClose(identity),
        pick("TerminalとPanelを閉じますか？\n\nシェルとログ取り込みは終了します。Panelに未保存の内容がある場合は保存してください。", "Close the terminal and its panels?\n\nShells and log capture will stop. Save any panel contents you want to keep.").into(),
        &[pick("Panelを保存して続行", "Save Panels and Continue"), pick("保存せず続行", "Continue Without Saving"), cancel()], 1);
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
        0 => id.update_screen(window, |s| {
            s.terminal_search_open = true;
            s.terminal_search_spot = if spot == TerminalSpot::Front { 0 } else { 1 };
        }),
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
        3 => id.update_screen(window, |s| s.terminal_search_open = false),
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

pub(crate) fn install(window: &AppWindow, live: &Live) {
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
