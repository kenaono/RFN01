//! 要件 8.5: 前の実行が置いていった配置を、次の実行が拾う。
//!
//! **本文はここに来ない。**未保存の中身は作業コピーが持っていて（`saving.rs`）、
//! こちらが持つのは「どのファイルがどのペインのどのタブに、どの向きとモードで、
//! どこを見て開いていたか」——**同じ文書についての、二つの別の問い**である。
//! 起動時に二つを突き合わせるのが[`open_session`]で、セッションが名を挙げた
//! ファイルに作業コピーが無ければ、それはディスクから開けばよい。
//!
//! **足し合わないものは黙って落とす。**消えたファイル、後の版が書いたペイン
//! 番号、ペインを1つも持たないセッション——**起動することのほうが、どれよりも
//! 価値がある**。
//!
//! 窓の位置と大きさもここ（要件 8.5、2026-08-27追加）。最大化していたかどうかも
//! 含めて、**最大化・最小化していない最後の姿**を覚える。

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use slint::{ComponentHandle, PhysicalPosition, PhysicalSize};

use crate::buffer::DocumentFile;
use crate::open_document::OpenDocument;
use crate::pane_layout::Layout;
use crate::{
    AppWindow, EditorState, Live, MAX_DOCUMENT_CHARACTERS, PaneId, PaneTab, PaneTabs,
    SAMPLE_MARKDOWN, TabBelow, TabView, Tabs, app_data, focused_pane, next_untitled_number,
};

/// Put back what the last run left (要件 8.5).
///
/// **The two halves come from different places and are matched up here.** The
/// text comes from the work copies (要件 8.1), because that is where anything
/// unsaved lives; the arrangement — which pane held which tab, in what order,
/// in which of the four modes, and how the area was divided — comes from the
/// session. A file the session names that no work copy holds had nothing
/// unsaved in it, so it is opened from disk.
///
/// Anything that does not add up is dropped rather than argued with: a file
/// that has been deleted, a pane number from a later build, a session with no
/// panes at all. **Starting is worth more than any of it.**
pub fn open_session(
    window: &AppWindow,
    session: Option<app_data::Session>,
    restored: Vec<(Rc<OpenDocument>, EditorState)>,
) -> (Tabs, Layout) {
    let Some(session) = session else {
        return open_without_session(window, restored);
    };
    let mut placed: Vec<Rc<OpenDocument>> = Vec::new();
    // **As many strips as the window has panes**, which is what the session's
    // own list said before the rows were published (要件 6.3, 8.5).
    let mut strips = PaneId::all(window)
        .iter()
        .map(|_| PaneTabs::default())
        .collect::<Vec<PaneTabs>>();
    // **Every stored strip lands somewhere.** Usually one for one, but a
    // session whose arrangement could not be restored has more strips than
    // there are panes, and the tabs of the ones past the end go into the last
    // pane rather than being dropped.
    let last = PaneId(strips.len() as u32 - 1);
    for (position, stored) in session.panes.iter().enumerate() {
        let id = PaneId::from_index(position as i32).min(last);
        for tab in &stored.tabs {
            let Some(document) = document_for(window, tab, &restored, &placed) else {
                continue;
            };
            if !placed.iter().any(|held| Rc::ptr_eq(held, &document)) {
                placed.push(document.clone());
            }
            let caret = tab.caret;
            strips[id.index() as usize].tabs.push(PaneTab {
                identity: Rc::new(()),
                // 追加要件 2026-09-15: セッションが覚えていたTABの紙の色。
                paper: from_stored(&tab.paper),
                tab_colour: tab
                    .tab_colour
                    .map(|[r, g, b]| slint::Color::from_rgb_u8(r, g, b)),
                // 要件 7.9（2026-09-08）: セッションが覚えていたモードの番号。
                // **無い番号は「なし」**になる（`word_mode_with`）——モードを
                // 消したあとの文書は、間違った色ではなく色無しで戻る。
                word_mode: tab.word_mode,
                document,
                view: TabView {
                    state: EditorState {
                        caret_source_byte: caret,
                        selection_anchor_source_byte: tab.anchor.or(caret),
                        ..EditorState::default()
                    },
                    scroll: tab.scroll as f32,
                    top: tab.top,
                    vertical: tab.vertical,
                    preview: tab.preview,
                },
                empty: tab.empty,
                settings: false,
                provisional: Cell::new(false),
                // A session remembers documents. **A shell is not one** — it is
                // a process that ended when the editor did, so a restored
                // terminal tab would be a name with nothing behind it.
                terminal: None,
                // **The strip comes back empty and opens itself when the tab
                // does.** Starting a shell for every restored tab at once would
                // be a dozen processes for a window showing one of them.
                below: TabBelow {
                    open: tab.below,
                    height: tab.below_height as f32,
                    ..TabBelow::default()
                },
            });
        }
        let strip = &mut strips[id.index() as usize];
        strip.active = stored.active.min(strip.tabs.len().saturating_sub(1));
        strip.paper = from_stored(&stored.paper);
    }

    // A work copy the session does not name is still somebody's unsaved work.
    // It goes in front of the writer rather than being left on disk unopened.
    let focused = PaneId::from_index(session.focused).min(PaneId(strips.len() as u32 - 1));
    for (document, state) in &restored {
        if placed.iter().any(|held| Rc::ptr_eq(held, document)) {
            continue;
        }
        let strip = &mut strips[focused.index() as usize];
        strip.tabs.push(PaneTab {
            identity: Rc::new(()),
            // セッションが名を挙げていない作業コピーなので、モードも無い。
            word_mode: 0,
            document: document.clone(),
            view: TabView {
                state: state.clone(),
                ..TabView::for_pane(window, focused)
            },
            terminal: None,
            below: TabBelow::default(),
            empty: false,
            settings: false,
            provisional: Cell::new(false),
            paper: [None; 2],
            tab_colour: None,
        });
    }
    for id in PaneId::all(window) {
        if strips[id.index() as usize].tabs.is_empty() {
            let number = next_untitled_number(&taken_numbers(&strips));
            let empty = OpenDocument::untitled(number, window.as_weak());
            strips[id.index() as usize]
                .tabs
                .push(PaneTab::showing(window, id, empty));
        }
    }
    // **Every pane the tree names has to exist**, and every pane that exists
    // has to be somewhere in the tree: a row nobody draws is a strip of tabs
    // the writer cannot reach, and a leaf naming nothing is an empty rectangle.
    // A session that fails either is one from another build, and the editor
    // opens on the focused pane alone rather than saying so.
    // 要件 7.9（書き手の報告 2026-09-08）: **前に出ているタブのモードを、画面の
    // 行へ入れておく。**組版はこの行から読む（`lay_out_pane`）のに、ここまでで
    // 入れているのはタブの側だけだった——**起動直後、ステータスバーには前回の
    // モード名が出ているのに色が付かない**のがそれである。名前はタブから、色は
    // 行から来ていて、二つが食い違っていた。
    for id in PaneId::all(window) {
        let mode = strips[id.index() as usize]
            .current()
            .map_or(0, |tab| tab.word_mode);
        id.update_screen(window, |screen| screen.word_mode = mode as i32);
    }
    let panes = strips.len();
    let layout = Layout::decode(&session.layout)
        .filter(|layout| {
            let named = layout.panes();
            named.len() == panes && (0..panes).all(|pane| named.contains(&pane))
        })
        .unwrap_or_else(|| Layout::single(focused.index() as usize));
    window.set_focused_pane(focused.index());
    (
        Tabs {
            panes: strips,
            no_tabs: Default::default(),
        },
        layout,
    )
}

/// The document one session tab names, from the work copies or from disk.
pub fn document_for(
    window: &AppWindow,
    tab: &app_data::SessionTab,
    restored: &[(Rc<OpenDocument>, EditorState)],
    placed: &[Rc<OpenDocument>],
) -> Option<Rc<OpenDocument>> {
    let names = |document: &Rc<OpenDocument>| {
        let file = document.file.borrow();
        match (&tab.origin, file.path()) {
            (Some(origin), Some(path)) => origin == path,
            (None, None) => file.untitled_number() == tab.untitled,
            _ => false,
        }
    };
    // A file open in two panes is one document, so a tab that names one already
    // put somewhere shares it (要件 7.6).
    if let Some(document) = placed.iter().find(|held| names(held)) {
        return Some(document.clone());
    }
    if let Some((document, _)) = restored.iter().find(|(document, _)| names(document)) {
        return Some(document.clone());
    }
    // Nothing unsaved, so the file itself is what it was.
    let origin = tab.origin.as_ref()?;
    match DocumentFile::open(origin, MAX_DOCUMENT_CHARACTERS) {
        Ok((file, text)) => Some(OpenDocument::new(file, text, window.as_weak())),
        Err(_) => None,
    }
}

/// Every untitled number the strips are holding.
pub fn taken_numbers(strips: &[PaneTabs]) -> Vec<u32> {
    strips
        .iter()
        .flat_map(|strip| strip.tabs.iter())
        .map(|tab| tab.document.file.borrow().untitled_number())
        .collect()
}

/// The first run, or one whose session could not be read.
///
/// The work copies go to the pane on screen, and the other pane opens on the
/// same document as its active tab — 要件 6.4's rule for a pane that is new.
pub fn open_without_session(
    window: &AppWindow,
    restored: Vec<(Rc<OpenDocument>, EditorState)>,
) -> (Tabs, Layout) {
    let documents = if restored.is_empty() {
        let first = OpenDocument::new(
            DocumentFile::untitled(1),
            SAMPLE_MARKDOWN.to_owned(),
            window.as_weak(),
        );
        vec![(first, EditorState::default())]
    } else {
        restored
    };
    let here = focused_pane(window);
    // **One pane** (要件 6.3). Nothing was restored, so there is no arrangement
    // to come back to, and an editor that opened divided would be dividing on
    // its own account.
    let strips = vec![PaneTabs {
        tabs: documents
            .iter()
            .map(|(document, state)| PaneTab {
                identity: Rc::new(()),
                word_mode: 0,
                document: document.clone(),
                view: TabView {
                    state: state.clone(),
                    ..TabView::for_pane(window, here)
                },
                terminal: None,
                below: TabBelow::default(),
                empty: false,
                settings: false,
                provisional: Cell::new(false),
                paper: [None; 2],
                tab_colour: None,
            })
            .collect(),
        active: 0,
        ..PaneTabs::default()
    }];
    (
        Tabs {
            panes: strips,
            no_tabs: Default::default(),
        },
        Layout::single(here.index() as usize),
    )
}

/// What is on screen, in the form the session keeps it (要件 8.5).
pub fn capture_session(window: &AppWindow, live: &Live) -> app_data::Session {
    let tabs = live.tabs.borrow();
    let panes = PaneId::all(window)
        .iter()
        .map(|id| {
            let strip = tabs.of(*id);
            app_data::SessionPane {
                active: strip
                    .tabs
                    .iter()
                    .take(strip.active)
                    .filter(|tab| !tab.stands_in() && !tab.document.read_only())
                    .count()
                    .min(
                        strip
                            .tabs
                            .iter()
                            .filter(|tab| !tab.stands_in() && !tab.document.read_only())
                            .count()
                            .saturating_sub(1),
                    ),
                zoom: id.zoom(window),
                paper: to_stored(&strip.paper),
                // **A shell is not written down** (追加要件 Terminal). The
                // process ends with the editor, so a remembered terminal tab
                // would come back as its stand-in document — an empty 無題
                // nobody asked for.
                tabs: strip
                    .tabs
                    .iter()
                    // 設定のTABも同じ（追加要件 2026-09-14）：開き直せば済み、
                    // 戻ってきても代役の空文書でしかない。
                    .filter(|tab| !tab.stands_in())
                    .filter(|tab| !tab.document.read_only())
                    .map(session_tab)
                    .collect(),
            }
        })
        .collect();
    let folder = live.folder.borrow().explorer_location();
    app_data::Session {
        layout: live.layout.borrow().encode(),
        // **最大化中は「元の大きさ」を持っていない**（[`window_place`]が`None`を返す）。
        // そのときは**前に書いてあった大きさを残す**（RFN01-45）——ここで`None`を書くと、
        // 最大化のまま終了しただけで覚えていた大きさが消え、次の起動は既定の小さな
        // 大きさで出る（そして「元の大きさに戻す」と、いよいよ小さくなる）。
        place: window_place(window).or_else(|| {
            app_data::read_session(&app_data::app_directory()?).and_then(|old| old.place)
        }),
        maximized: window.window().is_maximized(),
        focused: {
            let id = focused_pane(window);
            if id.is_panel() {
                id.screen(window).panel_owner
            } else {
                id.index()
            }
        },
        panes,
        folder: folder.root.clone(),
        search_folder: folder.searching.clone(),
        search_exclusions: window.get_search_exclusions().to_string(),
        shortcut_bindings: window.get_shortcut_bindings().to_string(),
        expanded: folder.expanded.iter().cloned().collect(),
        tree_shown: window.get_tree_open(),
        // 追加要件 2026-09-06: and how wide the writer left it.
        tree_width: (window.get_tree_width() / 1.0) as u32,
        recent: live.recent.borrow().clone(),
        folders: live.recent_folders.borrow().clone(),
        // E1の④: 打ち直さないための短い列。**書き出すのはここだけ**——語を
        // 覚えるたびにセッションを書けば、F3のたびにファイルが1つ書かれる。
        needles: live.find_terms.borrow().kept().to_vec(),
        replacements: live.replace_terms.borrow().kept().to_vec(),
    }
}

/// Put the window back where the writer left it (要件 8.5).
///
/// **A size that would not fit on any screen is refused**, which is the one
/// thing that can be said without asking Windows which screens there are: a
/// session carried to a machine with a smaller monitor, or written by a
/// hand-edit, must not open a window nobody can reach the edges of. Where it
/// sits is left alone — a window off the side of a screen is one drag away,
/// and second-guessing which monitor a writer meant is worse than obeying them.
///
/// **This pass is the one before the window is shown** (要件 8.5), and asking
/// Slint for a size goes through winit, which counts the caption this chrome
/// paints over. It opens in the right place and close to the right size, and
/// [`super::window_chrome::restore_place`] puts the client back exactly once the
/// window has a handle (RFN01-40).
pub fn restore_window_place(
    window: &AppWindow,
    place: Option<app_data::WindowPlace>,
    maximized: bool,
) {
    if let Some(place) =
        place.filter(|place| place.width <= MAX_WINDOW && place.height <= MAX_WINDOW)
    {
        let handle = window.window();
        handle.set_position(PhysicalPosition::new(place.x, place.y));
        handle.set_size(PhysicalSize::new(place.width, place.height));
    }
    if maximized {
        window.window().set_maximized(true);
    }
}

/// The largest window a session may ask for, in physical pixels.
///
/// Two 8K screens side by side and a margin. Anything past that is not a window
/// somebody left behind.
pub const MAX_WINDOW: u32 = 16_384;

/// Where the window is and how big, as Windows counts it (要件 8.5).
///
/// **Not while it is maximised or minimised.** Both report the size they are
/// filling rather than the size they would go back to, and a window restored to
/// a maximised size without being maximised is one that cannot be un-maximised.
/// The place kept is the last one the writer actually put it in.
///
/// The pair is the outer position and the **client** size — where the window
/// sits and how much room the document has. [`restore_window_place`] asks Slint
/// for both; [`super::window_chrome::restore_place`] puts the frame back on
/// around the client on the monitor the window landed on.
pub fn window_place(window: &AppWindow) -> Option<app_data::WindowPlace> {
    let handle = window.window();
    if handle.is_maximized() || handle.is_minimized() {
        return None;
    }
    let position = handle.position();
    let size = handle.size();
    (size.width > 0 && size.height > 0).then_some(app_data::WindowPlace {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    })
}

/// One tab, as the session keeps it.
///
/// The document is named the way a work copy names one — by its file, or by its
/// untitled number — so that the two are matched up when they come back.
pub fn session_tab(tab: &PaneTab) -> app_data::SessionTab {
    let file = tab.document.file.borrow();
    app_data::SessionTab {
        // 要件 7.9（2026-09-08）: この文書のモード。**名前で覚える**——番号は
        // モードを作り消しすれば別のものを指す。
        word_mode: tab.word_mode,
        origin: file.path().map(Path::to_path_buf),
        untitled: file.untitled_number(),
        vertical: tab.view.vertical,
        preview: tab.view.preview,
        // Whole pixels: the scroll is a position on a page, and a session that
        // remembered a fraction of one would be keeping precision nobody can
        // see.
        scroll: tab.view.scroll as i32,
        top: tab.view.top,
        caret: tab.view.state.caret_source_byte,
        anchor: tab.view.state.selection_anchor_source_byte,
        // 追加要件 Terminal: the strip is part of the arrangement, so it is part
        // of what 要件 8.5 puts back.
        below: tab.below.open,
        below_height: tab.below.height as i32,
        // 追加要件 2026-09-07: a tab that had not been asked yet comes back
        // asking.
        empty: tab.empty,
        paper: to_stored(&tab.paper),
        tab_colour: tab
            .tab_colour
            .map(|colour| [colour.red(), colour.green(), colour.blue()]),
    }
}

/// 追加要件 2026-09-15: 紙の色を、セッションが書く形へ。
fn to_stored(paper: &crate::Paper) -> app_data::Paper {
    paper.map(|held| held.map(|colour| [colour.red(), colour.green(), colour.blue()]))
}

fn from_stored(paper: &app_data::Paper) -> crate::Paper {
    paper.map(|held| held.map(|[r, g, b]| slint::Color::from_rgb_u8(r, g, b)))
}

/// Put the session away.
///
/// **Written whole, every time.** It is a few hundred bytes about an
/// arrangement, not a document, so there is nothing to be gained by working out
/// what changed.
pub fn write_session(window: &AppWindow, live: &Live) {
    let Some(directory) = app_data::app_directory() else {
        return;
    };
    let session = capture_session(window, live);
    let (runtime, expanded) = {
        let folder = live.folder.borrow();
        (
            folder.workspace.clone(),
            if folder.workspace_view {
                folder.expanded.clone()
            } else {
                folder.workspace_location.expanded.clone()
            },
        )
    };
    if let Some(runtime) = runtime {
        if let Err(error) = runtime.borrow_mut().persist_active_expanded(expanded) {
            live.cache
                .borrow_mut()
                .log_diag("workspace", &format!("view save failed error={error}"));
        }
    }
    if let Err(error) = app_data::write_session(&directory, &session) {
        live.cache
            .borrow_mut()
            .log_diag("session", &format!("save failed error={error}"));
    }
}
