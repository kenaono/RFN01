//! RFN01-87: the command palette — the frame in the middle of the title bar.
//!
//! **One field, two uses** (書き手の決定 2026-11-03, the way VS Code does it):
//! with nothing in front it finds a file by name (`Ctrl+P`), and with a leading
//! `>` it finds an operation (`Ctrl+Shift+P` opens it with the `>` in place).
//! Deleting the `>` goes back to files.
//!
//! - **Files** come from the Workspace's index while a Workspace is open, and
//!   from the Explorer's folder otherwise; the files opened lately come first.
//! - **Operations** are the title bar's menus, read as they would open now
//!   (`menu_commands::menu_snapshot`), and the operations that only have a key
//!   (`shortcuts::palette_actions`). An operation runs exactly as from the menu
//!   or the key — the same command, on the pane that was in front.
//!
//! What the palette was opened over is caught when it opens: the keyboard is
//! in the field by then, and the pane it acts on is the one the writer was in.
use super::*;
use std::path::{Path, PathBuf};

/// How many files a folder walk takes before it stops. A work folder is a few
/// hundred files; a folder this big is not one a name search should wait on.
const WALK_LIMIT: usize = 20_000;
/// How many rows the list shows. The rest are found by typing more.
const ROWS_SHOWN: usize = 200;

thread_local! {
    static PALETTE: RefCell<Option<Palette>> = const { RefCell::new(None) };
    static KILLS: RefCell<Option<Rc<RefCell<Kills>>>> = const { RefCell::new(None) };
}

struct Palette {
    pane: PaneId,
    /// Files in the order an empty search lists them: recent first.
    files: Option<Vec<FileCandidate>>,
    menu: Option<menu_commands::MenuSnapshot>,
    actions: Vec<shortcuts::PaletteAction>,
    shown: Vec<Shown>,
}

#[derive(Clone)]
struct FileCandidate {
    path: PathBuf,
    name: String,
    /// The folder it is in, as shown under the name: relative to the folder
    /// searched, or whole for a recent file outside it.
    folder: String,
    recent: bool,
}

#[derive(Clone, Copy)]
enum Shown {
    File(usize),
    Menu(usize),
    Action(usize),
}

pub fn install(window: &AppWindow, live: &Live, kills: &Rc<RefCell<Kills>>) {
    KILLS.with(|held| *held.borrow_mut() = Some(kills.clone()));
    let weak = window.as_weak();
    let opened = live.clone();
    window.on_palette_requested(move |commands| {
        if let Some(window) = weak.upgrade() {
            open(&window, &opened, commands);
        }
    });
    let weak = window.as_weak();
    let edited = live.clone();
    window.on_palette_edited(move |text| {
        if let Some(window) = weak.upgrade() {
            refilter(&window, &edited, &text);
        }
    });
    let weak = window.as_weak();
    let accepted = live.clone();
    window.on_palette_accepted(move || {
        if let Some(window) = weak.upgrade() {
            accept(&window, &accepted);
        }
    });
    let weak = window.as_weak();
    window.on_palette_cancelled(move || {
        if let Some(window) = weak.upgrade() {
            close(&window);
        }
    });
}

/// Open the palette: for operations when `commands`, for files otherwise. A
/// palette already open switches to what was asked for.
pub fn open(window: &AppWindow, live: &Live, commands: bool) {
    if window.get_title_menu_visible() {
        window.invoke_title_menu_dismissed();
    }
    if !window.get_palette_open() {
        let pane = focused_pane(window);
        PALETTE.with(|held| {
            *held.borrow_mut() = Some(Palette {
                pane,
                files: None,
                menu: None,
                actions: Vec::new(),
                shown: Vec::new(),
            })
        });
    }
    let text = if commands { "> " } else { "" };
    window.set_palette_text(text.into());
    window.set_palette_open(true);
    refilter(window, live, text);
    window.set_palette_focus_generation(window.get_palette_focus_generation() + 1);
}

/// Put the palette away and the keyboard back in the text.
pub fn close(window: &AppWindow) {
    if !window.get_palette_open() {
        return;
    }
    window.set_palette_open(false);
    window.set_palette_text("".into());
    window.set_palette_rows(ModelRc::from(Rc::new(VecModel::<PaletteRow>::default())));
    PALETTE.with(|held| held.borrow_mut().take());
    window.set_focus_generation(window.get_focus_generation() + 1);
}

fn refilter(window: &AppWindow, live: &Live, text: &str) {
    let rows = PALETTE.with(|held| {
        let mut held = held.borrow_mut();
        let palette = held.as_mut()?;
        Some(match text.strip_prefix('>') {
            Some(query) => {
                if palette.menu.is_none() {
                    let kills = KILLS.with(|kills| kills.borrow().clone())?;
                    palette.menu = Some(menu_commands::menu_snapshot(window, live, &kills));
                    palette.actions = shortcuts::palette_actions(window);
                }
                let menu = palette.menu.as_ref()?;
                let (rows, shown) = command_rows(menu, &palette.actions, query.trim());
                palette.shown = shown;
                rows
            }
            None => {
                if palette.files.is_none() {
                    palette.files = Some(file_candidates(live));
                }
                let files = palette.files.as_deref().unwrap_or_default();
                let (rows, shown) = file_rows(files, text.trim());
                palette.shown = shown;
                rows
            }
        })
    });
    let rows = rows.unwrap_or_default();
    let note = if rows.is_empty() {
        crate::i18n::pick("一致するものはありません", "No matches")
    } else {
        ""
    };
    window.set_palette_note(note.into());
    window.set_palette_selected(0);
    window.set_palette_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn accept(window: &AppWindow, live: &Live) {
    let selected = window.get_palette_selected();
    let Some((pane, chosen)) = PALETTE.with(|held| {
        let held = held.borrow();
        let palette = held.as_ref()?;
        let shown = *palette.shown.get(usize::try_from(selected).ok()?)?;
        let chosen = match shown {
            Shown::File(at) => Chosen::File(palette.files.as_ref()?.get(at)?.path.clone()),
            Shown::Menu(at) => Chosen::Menu(at),
            Shown::Action(at) => Chosen::Action(palette.actions.get(at)?.id),
        };
        Some((palette.pane, chosen))
    }) else {
        return;
    };
    // The menu snapshot holds the pane it was made for, so it is taken out
    // before the palette is put away.
    let menu = PALETTE.with(|held| held.borrow_mut().as_mut().and_then(|p| p.menu.take()));
    close(window);
    let weak = window.as_weak();
    let live = live.clone();
    // After the keyboard is back where it was: an operation acts on the focus.
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        match chosen {
            Chosen::File(path) => open_path_in_focused_pane(&window, &live, &path, Opening::Kept),
            Chosen::Menu(at) => {
                if let Some(menu) = &menu {
                    menu.run(&window, &live, at);
                }
            }
            Chosen::Action(id) => shortcuts::run_command(&window, &live, pane, id),
        }
    });
}

enum Chosen {
    File(PathBuf),
    Menu(usize),
    Action(i32),
}

/// The files a name search looks through, recent ones first.
fn file_candidates(live: &Live) -> Vec<FileCandidate> {
    let (roots, indexed) = search_scope(live);
    let roots: Vec<PathBuf> = roots.iter().map(|root| same_form(root)).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    // **探した登録フォルダの名前と、その中の場所**（例：`原稿\第1章`）。外にある最近の
    // ファイル（Explorerのとき）は、フォルダをそのまま。
    let folder_of = |path: &Path| -> String {
        let parent = path.parent().unwrap_or(Path::new(""));
        roots
            .iter()
            .find_map(|root| {
                let relative = parent.strip_prefix(root).ok()?;
                let name = root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                let name = name.unwrap_or_else(|| root.display().to_string());
                Some(if relative.as_os_str().is_empty() {
                    name
                } else {
                    format!("{name}\\{}", relative.display())
                })
            })
            .unwrap_or_else(|| parent.display().to_string())
    };
    // 書き手の求め 2026-11-03: **開けないファイルは出さない**——Workspaceを使っているあいだは
    // 登録フォルダの外のファイルを開かない（`workspace_boundary`）ので、最近開いたファイルも
    // その中のものだけにする。Explorerなら、どのファイルも開ける。
    let openable = |path: &Path| match &indexed {
        Some(_) => crate::workspace_boundary::existing_file_allowed(&roots, path),
        None => path.is_file(),
    };
    for path in live.recent.borrow().iter() {
        if !openable(path) {
            continue;
        }
        let path = same_form(path);
        if seen.insert(path.clone()) {
            out.push(FileCandidate {
                name: entry_name(&path),
                folder: folder_of(&path),
                path,
                recent: true,
            });
        }
    }
    let mut rest: Vec<PathBuf> = match indexed {
        // The index's paths are resolved already: only the prefix comes off.
        Some(entries) => entries.iter().map(|path| without_verbatim(path)).collect(),
        None => walk(&roots),
    };
    rest.sort_by_key(|path| entry_name(path).to_lowercase());
    for path in rest {
        if seen.insert(path.clone()) {
            out.push(FileCandidate {
                name: entry_name(&path),
                folder: folder_of(&path),
                path,
                recent: false,
            });
        }
    }
    out
}

/// A path in the one form every list here is compared in: resolved, and without
/// the `\\?\` Windows puts in front of a resolved path. **The index holds the
/// resolved form and the history the form it was opened by**, so the same file
/// was listed twice — once as a recent file and once from the index.
fn same_form(path: &Path) -> PathBuf {
    without_verbatim(&path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

fn without_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// Where files are looked for: the active Workspace's registered folders, with
/// its index, or the Explorer's folder, which is walked.
fn search_scope(live: &Live) -> (Vec<PathBuf>, Option<Vec<PathBuf>>) {
    let folder = live.folder.borrow();
    let workspace = folder.workspace.as_ref().and_then(|runtime| {
        let runtime = runtime.borrow();
        runtime.active_workspace().map(|_| runtime.active_roots())
    });
    if let Some(roots) = workspace {
        drop(folder);
        let held = workspace_link_ui(live);
        let entries = held
            .try_borrow()
            .map(|ui| {
                ui.index
                    .entries_shared()
                    .iter()
                    .map(|entry| entry.canonical.clone())
                    .collect()
            })
            .unwrap_or_default();
        let roots = roots
            .iter()
            .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
            .collect();
        return (roots, Some(entries));
    }
    let root = if folder.workspace_view {
        folder.explorer_location.root.clone()
    } else {
        folder.root.clone()
    };
    (root.into_iter().collect(), None)
}

/// Every file under `roots`, folders that hold no writing left out.
fn walk(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending: Vec<PathBuf> = roots.to_vec();
    while let Some(folder) = pending.pop() {
        let Ok(read) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if !skipped_folder(&name) {
                    pending.push(path);
                }
            } else if kind.is_file() {
                out.push(path);
                if out.len() >= WALK_LIMIT {
                    return out;
                }
            }
        }
    }
    out
}

/// Folders a name search does not go into: hidden ones (`.git`) and what tools
/// build. Nobody opens a manuscript from there.
fn skipped_folder(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules")
}

/// How well `query` matches `text`, higher better; `None` when it does not.
///
/// **The letters of the query in order** — `bunsho` finds `bunsho-memo` and
/// `第1章` finds `第1章_下書き`. A run of them together counts for more than
/// the same letters spread out, and a match at the start more than one later.
/// Case and full-width/half-width forms are not told apart.
pub(crate) fn score(query: &str, text: &str) -> Option<i64> {
    let query: Vec<char> = fold(query).chars().filter(|c| !c.is_whitespace()).collect();
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = fold(text).chars().collect();
    let joined: String = query.iter().collect();
    let whole: String = text.iter().collect();
    if let Some(at) = whole.find(&joined) {
        let at = whole[..at].chars().count() as i64;
        return Some(10_000 - at * 10 - text.len() as i64);
    }
    let mut points = 0i64;
    let mut next = 0usize;
    let mut previous: Option<usize> = None;
    for wanted in &query {
        let at = text[next..].iter().position(|c| c == wanted)? + next;
        points += match previous {
            Some(before) if before + 1 == at => 30,
            _ => 5,
        };
        points -= at as i64;
        previous = Some(at);
        next = at + 1;
    }
    Some(points)
}

/// 同じ操作かを名前で見るときの形。**末尾の「…」は見ない**——メニューの「行へ移動…」と
/// キーの一覧の「行へ移動」は同じ操作である。
fn same_name(text: &str) -> String {
    fold(text.trim_end_matches(['…', '.']))
}

fn fold(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap_or(c),
            '\u{3000}' => ' ',
            _ => c,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

fn file_rows(files: &[FileCandidate], query: &str) -> (Vec<PaletteRow>, Vec<Shown>) {
    let mut found: Vec<(i64, usize)> = files
        .iter()
        .enumerate()
        .filter_map(|(at, file)| {
            if query.is_empty() {
                return Some((0, at));
            }
            let by_name = score(query, &file.name);
            // The folder only counts when the letters are there together: spread
            // over a long path, any three letters are found somewhere.
            let by_path = fold(&file.folder).contains(&fold(query)).then_some(-5_000);
            let best = by_name.max(by_path)?;
            Some((best + if file.recent { 50 } else { 0 }, at))
        })
        .collect();
    if !query.is_empty() {
        // Stable, so equal scores keep the recent-first order.
        found.sort_by_key(|(points, _)| std::cmp::Reverse(*points));
    }
    found.truncate(ROWS_SHOWN);
    let rows = found
        .iter()
        .map(|(_, at)| {
            let file = &files[*at];
            PaletteRow {
                title: file.name.clone().into(),
                detail: file.folder.clone().into(),
                key: if file.recent {
                    crate::i18n::pick("最近", "recent").into()
                } else {
                    SharedString::default()
                },
            }
        })
        .collect();
    (
        rows,
        found.into_iter().map(|(_, at)| Shown::File(at)).collect(),
    )
}

fn command_rows(
    menu: &menu_commands::MenuSnapshot,
    actions: &[shortcuts::PaletteAction],
    query: &str,
) -> (Vec<PaletteRow>, Vec<Shown>) {
    let mut found: Vec<(i64, Shown, PaletteRow)> = Vec::new();
    let mut names = std::collections::HashSet::new();
    for (at, entry) in menu.entries.iter().enumerate() {
        if !entry.enabled {
            continue;
        }
        let leaf = entry.title.rsplit(" › ").next().unwrap_or(&entry.title);
        names.insert(same_name(leaf));
        let points = score(query, &entry.title).or_else(|| score(query, &entry.other));
        if let Some(points) = points {
            let (upper, lower) = two_lines(&entry.title, &entry.other);
            found.push((
                points,
                Shown::Menu(at),
                PaletteRow {
                    title: upper.into(),
                    detail: lower.into(),
                    key: entry.key.clone().into(),
                },
            ));
        }
    }
    for (at, action) in actions.iter().enumerate() {
        if names.contains(&same_name(&action.title)) {
            continue;
        }
        let points = score(query, &action.title).or_else(|| score(query, &action.other));
        if let Some(points) = points {
            let (upper, lower) = two_lines(&action.title, &action.other);
            found.push((
                points,
                Shown::Action(at),
                PaletteRow {
                    title: upper.into(),
                    detail: lower.into(),
                    key: action.key.clone().into(),
                },
            ));
        }
    }
    if !query.is_empty() {
        found.sort_by_key(|(points, _, _)| std::cmp::Reverse(*points));
    }
    found.truncate(ROWS_SHOWN);
    found
        .into_iter()
        .map(|(_, shown, row)| (row, shown))
        .unzip()
}

/// RFN01-87（書き手の決定 2026-11-03）: **上の段に表示の言葉、下の段にもう一方の言葉。**
/// 日本語の画面では上が日本語・下が英語、英語の画面では上が英語・下が日本語。探すのは
/// どちらの言葉でも。言葉が増えたら、下の段は英語にする（いまは日本語と英語だけ）。
///
/// **英語の画面では英語の1段だけ**（書き手の決定 2026-11-03）。下の段の日本語は、
/// 英語で使う人には読めない字が並ぶだけになる。探すのは、それでも両方の言葉で。
fn two_lines(shown: &str, other: &str) -> (String, String) {
    let english_shown = crate::i18n::pick("ja", "en") == "en";
    if english_shown || other.is_empty() || other == shown {
        return (shown.to_owned(), String::new());
    }
    (shown.to_owned(), other.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFN01-87: the letters in order, together counting for more.
    #[test]
    fn a_name_search_takes_the_letters_in_order() {
        assert!(score("memo", "memo.md").is_some());
        assert!(score("mmd", "memo.md").is_some());
        assert!(score("dm", "memo.md").is_none());
        // Together beats spread out, and the start beats later on.
        assert!(score("memo", "memo.md") > score("memo", "my-emo.md"));
        assert!(score("章", "第1章.md") < score("第", "第1章.md"));
        // Case and full-width forms are the same letters.
        assert!(score("ＭＥＭＯ", "Memo.md").is_some());
        assert_eq!(score("", "anything"), Some(0));
    }

    /// RFN01-87: the index's `\\?\D:\…` and the history's `D:\…` are one file.
    #[test]
    fn a_resolved_path_loses_its_prefix() {
        assert_eq!(
            without_verbatim(Path::new(r"\\?\D:\原稿\第1章.md")),
            PathBuf::from(r"D:\原稿\第1章.md")
        );
        assert_eq!(
            without_verbatim(Path::new(r"\\?\UNC\server\share\a.md")),
            PathBuf::from(r"\\?\UNC\server\share\a.md")
        );
        assert_eq!(
            without_verbatim(Path::new(r"D:\a.md")),
            PathBuf::from(r"D:\a.md")
        );
    }

    /// RFN01-87: an empty search keeps the recent-first order; a search
    /// ranks, a recent file winning a tie.
    #[test]
    fn recent_files_lead_and_a_search_ranks() {
        let file = |name: &str, recent: bool| FileCandidate {
            path: PathBuf::from(name),
            name: name.to_owned(),
            folder: String::new(),
            recent,
        };
        let files = vec![
            file("b-notes.md", true),
            file("a-notes.md", false),
            file("chapter.md", false),
        ];
        let (rows, _) = file_rows(&files, "");
        assert_eq!(rows[0].title, "b-notes.md");
        assert_eq!(rows.len(), 3);
        let (rows, _) = file_rows(&files, "chap");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "chapter.md");
        let (rows, _) = file_rows(&files, "notes");
        assert_eq!(rows[0].title, "b-notes.md", "the recent one wins the tie");
    }
}
