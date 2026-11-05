//! RFN01-71・RFN01-73: TABを閉じる範囲と、検索結果の印を、画面の値で確かめる。
use super::*;
use crate::saving::{Harness, open_under, scratch_directory};
use slint::Model;

fn names(live: &Live, id: PaneId) -> Vec<String> {
    live.tabs
        .borrow()
        .of(id)
        .tabs
        .iter()
        .map(|tab| tab.document.file.borrow().title())
        .collect()
}

/// a.md・b.md を左のPaneに、b.md（分割で写ったもの）・c.md を右のPaneに。
fn two_panes(root: &Path) -> (Harness, PaneId, PaneId) {
    let (harness, _) = Harness::new(|weak| open_under(root, "a.md", "あ\n", weak));
    let (window, live) = (&harness.window, &harness.live);
    let (left, right) = (PaneId::from_index(0), PaneId::from_index(1));
    let b = open_under(root, "b.md", "い\n", window.as_weak());
    add_tab(window, live, left, PaneTab::showing(window, left, b));
    divide_pane(window, live, left, Split::SideBySide);
    let c = open_under(root, "c.md", "う\n", window.as_weak());
    add_tab(window, live, right, PaneTab::showing(window, right, c));
    assert_eq!(names(live, left), ["a.md", "b.md"]);
    assert_eq!(names(live, right), ["b.md", "c.md"]);
    (harness, left, right)
}

/// RFN01-71: 他のTAB・すべてのTABを閉じるは、押したPaneの中だけを閉じる。
#[test]
fn closing_other_tabs_stays_in_the_pane_that_asked() {
    let root = scratch_directory("close-run-pane");
    let (harness, left, right) = two_panes(&root);
    let (window, live) = (&harness.window, &harness.live);

    start_close_run(window, live, left, true);
    assert_eq!(names(live, left), ["b.md"], "the tab in front is kept");
    assert_eq!(
        names(live, right),
        ["b.md", "c.md"],
        "the other pane is left alone"
    );

    start_close_run(window, live, right, false);
    assert_eq!(
        live.layout.borrow().panes(),
        vec![0],
        "an emptied pane is undivided"
    );
    assert_eq!(names(live, left), ["b.md"]);
}

/// RFN01-73: 検索結果では、フォーカスのあるPaneで前に出ている文書のファイルの行に印を付ける。
#[test]
fn search_results_mark_the_file_in_front_of_each_pane() {
    let root = scratch_directory("search-mark");
    let (harness, left, _) = two_panes(&root);
    let (window, live) = (&harness.window, &harness.live);
    let row = |name: &str, under_file: bool, verbatim: bool| {
        let path = root.join(name);
        let path = if verbatim {
            PathBuf::from(format!(r"\\?\{}", path.display()))
        } else {
            path
        };
        ResultRow {
            text: name.to_owned(),
            under_file,
            path,
            at: 0,
        }
    };
    // b.md は`\\?\`付きで返ってきても同じファイルとして扱う（Workspaceの検索はそう返す）。
    *live.results.borrow_mut() = vec![
        row("c.md", false, false),
        row("c.md", true, false),
        row("b.md", false, true),
        row("b.md", true, true),
    ];
    let marks =
        |window: &AppWindow| -> Vec<i32> { window.get_search_pane_file_indices().iter().collect() };

    window.set_left_tab(LeftTab::Search.index());
    publish_results(window, live);
    assert_eq!(
        marks(window),
        [2, 0],
        "the file rows, never a line under one"
    );

    // TABを替えれば印も移る。結果に無いファイルには付けない。
    let tab = live.tabs.borrow().of(left).tabs[0].clone();
    live.tabs.borrow_mut().of_mut(left).unwrap().active = 0;
    live.show_tab(window, left, &tab);
    publish_tabs(window, live);
    assert_eq!(marks(window), [-1, 0]);

    // ほかの面を見ているあいだは何も指さない。
    window.set_left_tab(LeftTab::Explorer.index());
    publish_tabs(window, live);
    assert_eq!(marks(window), [-1, -1]);
}

/// 書き手の求め 2026-11-05: Workspaceを使っているあいだ、Explorerは使えない。どの道から
/// Explorerを選んでも、左ペインはWorkspaceの表示に替わる。使っていなければExplorerのまま。
#[test]
fn the_explorer_gives_way_to_a_workspace_in_use() {
    let root = scratch_directory("explorer-workspace");
    let (harness, _) = Harness::new(|weak| open_under(&root, "a.md", "あ\n", weak));
    let (window, live) = (&harness.window, &harness.live);
    window.set_workspace_active(false);
    window.set_left_tab(LeftTab::Explorer.index());
    publish_left(window, live);
    assert_eq!(window.get_left_tab(), LeftTab::Explorer.index());

    window.set_workspace_active(true);
    window.set_left_tab(LeftTab::Explorer.index());
    publish_left(window, live);
    assert_eq!(window.get_left_tab(), LeftTab::Workspace.index());
    // ほかの面はそのまま。
    window.set_left_tab(LeftTab::Search.index());
    publish_left(window, live);
    assert_eq!(window.get_left_tab(), LeftTab::Search.index());
}

/// RFN01-108: `Ctrl+J`はただの改行。箇条書きの行でも印を継がない（Enterは継ぐ）。
#[test]
fn ctrl_j_breaks_the_line_without_carrying_the_list_mark() {
    let root = scratch_directory("ctrl-j");
    let (harness, _) = Harness::new(|weak| open_under(&root, "a.md", "- 項目\n", weak));
    let (window, live) = (&harness.window, &harness.live);
    let id = PaneId::from_index(0);
    let document = live.states.document(id);
    let at_end_of_item = "- 項目".len();
    live.states.of(id).borrow_mut().caret_source_byte = Some(at_end_of_item);
    enter_in_pane(window, live, id, false);
    assert_eq!(
        &*document.text.borrow(),
        "- 項目\n- \n",
        "Enter carries the mark"
    );
    undo_in_pane(window, id, &document, &live.states, &live.cache, false);
    assert_eq!(&*document.text.borrow(), "- 項目\n");
    live.states.of(id).borrow_mut().caret_source_byte = Some(at_end_of_item);
    plain_enter_in_pane(window, live, id);
    assert_eq!(&*document.text.borrow(), "- 項目\n\n", "Ctrl+J does not");
}

/// RFN01-66（書き手の判断 2026-11-05）: ヘルプ。サンプル文書は無題で開き、元の文書は残る。
/// 「RFN Editについて」は問いの窓に版を出す。「キーの一覧」は設定のKeysの面。
#[test]
fn help_opens_the_sample_the_keys_and_about() {
    let root = scratch_directory("help");
    let (harness, _) = Harness::new(|weak| open_under(&root, "a.md", "あ\n", weak));
    let (window, live) = (&harness.window, &harness.live);
    let id = PaneId::FIRST;
    open_sample_document(window, live, id);
    let tab = live.tabs.borrow().of(id).current().cloned().unwrap();
    assert!(tab.document.file.borrow().path().is_none(), "untitled");
    assert!(tab.document.text.borrow().contains("灯台守の手紙"));
    assert_eq!(names(live, id), ["a.md", "無題1"], "the open file stays");

    show_about(window, live);
    assert!(matches!(*live.pending.borrow(), Some(Question::About)));
    assert!(
        window
            .get_question_text()
            .contains(env!("CARGO_PKG_VERSION"))
    );
    answer_question(window, live, 0);
    assert!(live.pending.borrow().is_none());

    open_settings_page(window, live, 6);
    assert_eq!(window.get_settings_tab(), 6);
    assert!(live.tabs.borrow().of(id).current().unwrap().settings);
}

/// 書き手の報告 2026-11-05（起動に失敗する）の元: 後ろにいたTABが覚えている位置は、同じ文書が
/// 前のTABで書き換えられても直されない。戻ったとき、文字の途中や本文の外を指したまま置かれ、
/// 保存されて次の起動で落ちていた。**戻すときに文字の境目へそろえる**（起動で戻すときと同じ）。
#[test]
fn a_tab_brought_back_after_an_edit_elsewhere_points_between_characters() {
    let root = scratch_directory("stale-view");
    let (harness, _) =
        Harness::new(|weak| open_under(&root, "a.md", "あいうえおかきくけこ\n", weak));
    let (window, live) = (&harness.window, &harness.live);
    let id = PaneId::FIRST;
    {
        let state = live.states.of(id);
        let mut state = state.borrow_mut();
        state.caret_source_byte = Some(27);
        state.selection_anchor_source_byte = Some(30);
    }
    duplicate_tab(window, live, id, 0);
    assert_eq!(live.tabs.borrow().of(id).active, 1);
    // 前のTAB（同じ文書）で、先頭の1字を消す。後ろのTABの位置（27・30）は字の途中になる。
    let document = live
        .tabs
        .borrow()
        .of(id)
        .current()
        .unwrap()
        .document
        .clone();
    *document.text.borrow_mut() = "いうえおかきくけこ\n".into();
    switch_to_tab(window, live, id, 0);
    assert_eq!(live.tabs.borrow().of(id).active, 0);
    let state = live.states.of(id);
    let state = state.borrow();
    let text = document.text.borrow();
    for byte in [state.caret_source_byte, state.selection_anchor_source_byte]
        .into_iter()
        .flatten()
    {
        assert!(text.is_char_boundary(byte), "{byte} inside a character");
    }
}
