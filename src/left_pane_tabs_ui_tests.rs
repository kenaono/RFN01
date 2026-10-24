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
