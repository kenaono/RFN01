//! RFN01-87: the command palette — the frame in the middle of the title bar.
//!
//! **One field, two uses** (書き手の決定 2026-11-03, the way VS Code does it):
//! with nothing in front it finds a file by name (`Ctrl+P`), and with a leading
//! `>` it finds an operation (`Ctrl+Shift+P` opens it with the `>` in place).
//! Deleting the `>` goes back to files.
//!
//! - **Files** come from the Workspace's index while a Workspace is open, and
//!   from the Explorer's folder otherwise; the files opened lately come first.
//! - **Operations** are the rows of the action table (`crate::actions`, RFN01-114)
//!   that can run now, each with its title and its description under it, and
//!   the rows of the writer's own data (templates, recent folders, word check
//!   modes, shells). An operation runs through the one entry
//!   (`menu_commands::run`), on the pane that was in front.
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
}

struct Palette {
    pane: PaneId,
    tree: menu_commands::Target,
    /// Files in the order an empty search lists them: recent first.
    files: Option<Vec<FileCandidate>>,
    /// RFN01-114: the operations that can run now, read when first asked.
    operations: Option<Vec<Operation>>,
    /// RFN01-88: the templates of the Workspace in use, read when first asked.
    templates: Option<Vec<crate::template::Template>>,
    /// RFN01-88・114: opened to choose one of a list (Insert ▸ Template…, or a
    /// generic action such as Open Recent Folder…), the list and nothing else.
    only: Option<List>,
    shown: Vec<Shown>,
}

/// RFN01-114（書き手と合意 2026-11-05）: 選ぶ一覧。データで中身が変わる項目は、汎用の
/// アクションがこの一覧をパレットに出す（キーはそのアクションに割り当てる）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum List {
    Templates,
    Recent,
    WordModes,
    Shells,
}

/// パレットの1行の操作。
struct Operation {
    category: crate::actions::Category,
    title: (String, String),
    description: (String, String),
    key: String,
    command: menu_commands::Command,
    /// アクションの表の行（キーを割り当てられる）。データの行は`None`。
    action: Option<usize>,
}

#[derive(Clone)]
struct FileCandidate {
    path: PathBuf,
    name: String,
    /// The folder it is in, as shown under the name: relative to the folder
    /// searched, or whole for a recent file outside it.
    folder: String,
    recent: bool,
    /// 探す範囲（開いているフォルダ・登録フォルダ）の中か。外にあるのは、Explorerの
    /// ときの最近のファイルだけ。
    inside: bool,
}

#[derive(Clone)]
enum Shown {
    File(usize),
    Operation(usize),
    Template(usize),
    /// RFN01-107: 設定をこの語で探す行。
    Settings,
    SettingItem(&'static crate::settings_items::Item),
    SettingGroup(crate::settings_items::Group),
}

// Keep only state and phase markers here. A palette can contain unpublished
// text and paths, and recording them is unnecessary for crash diagnosis.
fn trace(live: &Live, category: &str, fields: &str) {
    if let Ok(mut cache) = live.cache.try_borrow_mut() {
        cache.log_diag(category, fields);
    }
}

pub fn install(window: &AppWindow, live: &Live) {
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
    let assigned = live.clone();
    window.on_palette_assign(move |index| {
        if let Some(window) = weak.upgrade() {
            assign(&window, &assigned, index);
        }
    });
    let weak = window.as_weak();
    let cancelled = live.clone();
    window.on_palette_cancelled(move || {
        if let Some(window) = weak.upgrade() {
            trace(&cancelled, "palette", "stage=cancel");
            close(&window);
        }
    });
    let focus = live.clone();
    window.on_palette_focus_trace(move |returned| {
        trace(
            &focus,
            "palette",
            if returned { "stage=field-focus-returned" } else { "stage=field-focus-begin" },
        );
    });
}

/// Open the palette: for operations when `commands`, for files otherwise. A
/// palette already open switches to what was asked for.
pub fn open(window: &AppWindow, live: &Live, commands: bool) {
    trace(
        live,
        "palette",
        &format!(
            "stage=open-begin mode={} pane={} panes={} left_open={} left_tab={} already_open={}",
            if commands { "commands" } else { "files" },
            window.get_focused_pane(),
            PaneId::count(window),
            window.get_tree_open(),
            window.get_left_tab(),
            window.get_palette_open(),
        ),
    );
    if window.get_title_menu_visible() {
        window.invoke_title_menu_dismissed();
    }
    if !window.get_palette_open() {
        let pane = focused_pane(window);
        PALETTE.with(|held| {
            *held.borrow_mut() = Some(Palette {
                pane,
                tree: menu_commands::Target::for_panels(window, live),
                files: None,
                operations: None,
                templates: None,
                only: None,
                shown: Vec::new(),
            })
        });
    }
    PALETTE.with(|held| {
        if let Some(palette) = held.borrow_mut().as_mut() {
            palette.only = None;
        }
    });
    let text = if commands { "> " } else { "" };
    window.set_palette_text(text.into());
    window.set_palette_open(true);
    prepare_open(window, live, text);
}

fn prepare_open(window: &AppWindow, live: &Live, text: &str) {
    trace(live, "palette", "stage=initial-rows-begin");
    refilter(window, live, text);
    trace(live, "palette", &format!(
        "stage=initial-rows-published rows={}", window.get_palette_rows().row_count()
    ));
    trace(live, "palette", "stage=focus-request");
    window.set_palette_focus_generation(window.get_palette_focus_generation() + 1);
}

/// RFN01-88 (Insert ▸ Template…): the palette with the templates only, for
/// `pane` — the one the template goes into.
pub fn open_templates(window: &AppWindow, live: &Live, pane: PaneId) {
    open_list_for(window, live, pane, List::Templates);
}

/// RFN01-114: the palette with one list only (Open Recent Folder…, Choose Word
/// Check Mode…, New Terminal (Choose Shell)…), for the pane in front.
pub(crate) fn open_list(window: &AppWindow, live: &Live, list: List) {
    open_list_for(window, live, focused_pane(window), list);
}

fn open_list_for(window: &AppWindow, live: &Live, pane: PaneId, list: List) {
    trace(live, "palette", "stage=open-begin mode=list");
    if window.get_title_menu_visible() {
        window.invoke_title_menu_dismissed();
    }
    PALETTE.with(|held| {
        *held.borrow_mut() = Some(Palette {
            pane,
            tree: menu_commands::Target::for_panels(window, live),
            files: None,
            operations: None,
            templates: None,
            only: Some(list),
            shown: Vec::new(),
        })
    });
    window.set_palette_text("".into());
    window.set_palette_open(true);
    prepare_open(window, live, "");
    // **Asked again after the text has taken the keyboard back**: chosen from
    // the palette itself ("> Insert › Template…"), the palette that closed
    // has just sent the keyboard to the text, and that lands after this.
    let weak = window.as_weak();
    let live = live.clone();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        if let Some(window) = weak.upgrade() {
            if window.get_palette_open() {
                trace(&live, "palette", "stage=focus-request retry=true");
                window.set_palette_focus_generation(window.get_palette_focus_generation() + 1);
            }
        }
    });
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

fn setting_score(query: &str, titles: &(String, String)) -> i64 {
    let (ja, en) = titles;
    let bare_ja = ja.strip_prefix("設定 › ").unwrap_or(ja);
    let bare_en = en.strip_prefix("Settings › ").unwrap_or(en);
    best_score(query, bare_ja, bare_en)
        .into_iter()
        .chain(best_score(query, ja, en))
        .max()
        .unwrap_or(0)
}

fn setting_row(titles: &(String, String), key: &str) -> PaletteRow {
    let (ja, en) = titles;
    let (title, detail) = if crate::i18n::japanese() {
        two_lines(ja, en)
    } else {
        two_lines(en, ja)
    };
    PaletteRow {
        title: title.into(),
        detail: detail.into(),
        key: key.into(),
        assign: SharedString::new(),
        ..Default::default()
    }
}

fn refilter(window: &AppWindow, live: &Live, text: &str) {
    trace(live, "palette.detail", "stage=filter-begin");
    let rows = PALETTE.with(|held| {
        let mut held = held.borrow_mut();
        let palette = held.as_mut()?;
        if let Some(list) = palette.only {
            if list == List::Templates {
                let templates = palette
                    .templates
                    .get_or_insert_with(|| crate::templates_in_use(live));
                let (rows, shown) = finish_ranked(ranked_templates(templates, text.trim(), false));
                palette.shown = shown;
                return Some(rows);
            }
            let operations = palette
                .operations
                .get_or_insert_with(|| data_operations(window, live, Some(list)));
            let (rows, shown) = finish_ranked(ranked_operations(operations, text.trim(), false));
            palette.shown = shown;
            return Some(rows);
        }
        Some(match text.strip_prefix('>') {
            Some(query) => {
                let pane = palette.pane;
                let operations = palette
                    .operations
                    .get_or_insert_with(|| operations_now(window, live, pane, &palette.tree));
                let mut ranked = ranked_operations(operations, query.trim(), true);
                // RFN01-88: each template is an operation of its own here.
                let templates = palette
                    .templates
                    .get_or_insert_with(|| crate::templates_in_use(live));
                ranked.extend(ranked_templates(templates, query.trim(), true));
                let settings_category = crate::actions::Category::ALL.len() as i32;
                let folded_query = crate::fold_for_search(query.trim());
                if !folded_query.is_empty() {
                    for group in crate::settings_items::Group::all(window)
                        .into_iter()
                        .filter(|group| group.matches(&folded_query))
                    {
                        let points = setting_score(query, &group.titles);
                        let row = setting_row(&group.titles, "");
                        ranked.push((points, settings_category, Shown::SettingGroup(group), row));
                    }
                    for item in crate::settings_items::Item::all()
                        .filter(|item| item.matches(&folded_query))
                    {
                        let key = item.toggle.map_or("", |toggle| {
                            if toggle.current(window) { "ON" } else { "OFF" }
                        });
                        let titles = item.titles();
                        let points = setting_score(query, &titles);
                        ranked.push((
                            points,
                            settings_category,
                            Shown::SettingItem(item),
                            setting_row(&titles, key),
                        ));
                    }
                }
                let (mut rows, mut shown) = finish_ranked(ranked);
                // RFN01-107（書き手と合意 2026-11-05）: 設定の項目は、設定の画面の検索で探す。
                // 語があれば最後に1行、選べば設定のTABがその語で絞られて開く。
                if !query.trim().is_empty() {
                    rows.push(PaletteRow {
                        title: format!(
                            "{}{}{}",
                            crate::i18n::pick("設定で「", "Search Settings for \u{201c}"),
                            query.trim(),
                            crate::i18n::pick("」を探す", "\u{201d}")
                        )
                        .into(),
                        detail: SharedString::new(),
                        key: SharedString::new(),
                        assign: SharedString::new(),
                        ..Default::default()
                    });
                    shown.push(Shown::Settings);
                }
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
    let mut rows = rows.unwrap_or_default();
    let query = text.strip_prefix('>').unwrap_or(text).trim();
    if !query.is_empty() {
        for row in &mut rows {
            row.title_parts = highlight(query, &row.title);
            row.detail_parts = highlight(query, &row.detail);
        }
    }
    let note = if rows.is_empty() {
        crate::i18n::pick("一致するものはありません", "No matches")
    } else {
        ""
    };
    trace(live, "palette.detail", &format!("stage=publish-begin rows={}", rows.len()));
    window.set_palette_note(note.into());
    window.set_palette_selected(0);
    window.set_palette_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    trace(live, "palette.detail", "stage=publish-returned");
}

fn accept(window: &AppWindow, live: &Live) {
    let selected = window.get_palette_selected();
    let Some((pane, tree, chosen)) = PALETTE.with(|held| {
        let held = held.borrow();
        let palette = held.as_ref()?;
        let shown = palette.shown.get(usize::try_from(selected).ok()?)?.clone();
        let chosen = match shown {
            Shown::File(at) => Chosen::File(palette.files.as_ref()?.get(at)?.path.clone()),
            Shown::Operation(at) => {
                let operation = palette.operations.as_ref()?.get(at)?;
                Chosen::Operation(
                    operation.command.clone(),
                    crate::i18n::pick(&operation.title.0, &operation.title.1).to_owned(),
                )
            }
            Shown::Template(at) => {
                Chosen::Template(palette.templates.as_ref()?.get(at)?.path.clone())
            }
            Shown::SettingItem(item) => Chosen::SettingItem(item),
            Shown::SettingGroup(group) => Chosen::SettingGroup(group),
            Shown::Settings => Chosen::Settings(
                window
                    .get_palette_text()
                    .trim_start_matches('>')
                    .trim()
                    .to_owned(),
            ),
        };
        Some((palette.pane, palette.tree.clone(), chosen))
    }) else {
        return;
    };
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
            Chosen::Operation(command, name) => {
                let target = if matches!(command, menu_commands::Command::Tree(_) | menu_commands::Command::PanelAction(_)) {
                    tree
                } else {
                    menu_commands::Target::for_pane(&window, &live, pane)
                };
                menu_commands::run(&window, &live, &target, command, &name);
            }
            Chosen::Template(path) => crate::insert_template(&window, &live, pane, &path),
            Chosen::SettingItem(item) => {
                if let Some(toggle) = item.toggle {
                    toggle.flip(&window);
                } else {
                    crate::open_setting_item(&window, &live, item);
                }
            }
            Chosen::SettingGroup(group) => group.open(&window, &live),
            Chosen::Settings(query) => {
                crate::open_settings_searching(&window, &live, &query, false)
            }
        }
    });
}

/// RFN01-107（書き手と合意 2026-11-05）: 行の鍵の印。設定のKeysを、その操作の名前で絞って開く
/// ——割り当ては今のKeysの画面で行う（VS Codeの歯車と同じ考え方）。
///
/// RFN01-114: **その操作の行だけ**に絞って開く（名前の字で探し直さない）。
fn assign(window: &AppWindow, live: &Live, index: i32) {
    let id = usize::try_from(index)
        .ok()
        .and_then(|at| window.get_palette_rows().row_data(at))
        .map(|row| row.assign.to_string())
        .unwrap_or_default();
    let Some(at) = crate::actions::index_of(&id) else {
        return;
    };
    close(window);
    let weak = window.as_weak();
    let live = live.clone();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        if let Some(window) = weak.upgrade() {
            crate::open_keys_for(&window, &live, at);
        }
    });
}

enum Chosen {
    File(PathBuf),
    Operation(menu_commands::Command, String),
    Template(PathBuf),
    Settings(String),
    SettingItem(&'static crate::settings_items::Item),
    SettingGroup(crate::settings_items::Group),
}

/// RFN01-88: the templates that match `query`. Among the operations (`named`)
/// each is called "Insert Template: name"; listed alone, by its name.
fn ranked_templates(
    templates: &[crate::template::Template],
    query: &str,
    named: bool,
) -> Vec<RankedRow> {
    let mut found: Vec<RankedRow> = Vec::new();
    for (at, template) in templates.iter().enumerate() {
        let (title, other) = if named {
            let ja = format!("挿入: テンプレートを挿入: {}", template.name);
            let en = format!("Insert: Insert Template: {}", template.name);
            if crate::i18n::japanese() {
                (ja, en)
            } else {
                (en, ja)
            }
        } else {
            (template.name.clone(), String::new())
        };
        let points = best_score(query, &title, &other);
        if let Some(points) = points {
            let (upper, lower) = two_lines(&title, &other);
            found.push((
                points,
                crate::actions::Category::Insert.index(),
                Shown::Template(at),
                PaletteRow {
                    title: upper.into(),
                    detail: lower.into(),
                    key: SharedString::new(),
                    assign: SharedString::new(),
                    ..Default::default()
                },
            ));
        }
    }
    found
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
                inside: roots.iter().any(|root| path.starts_with(root)),
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
                inside: true,
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
    score_match(query, text, |_| {})
}

fn score_match(query: &str, text: &str, mut matched: impl FnMut(usize)) -> Option<i64> {
    let query: Vec<char> = fold(query).chars().filter(|c| !c.is_whitespace()).collect();
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = fold(text).chars().collect();
    let joined: String = query.iter().collect();
    let whole: String = text.iter().collect();
    if let Some(at) = whole.find(&joined) {
        let at = whole[..at].chars().count() as i64;
        (at as usize..at as usize + query.len()).for_each(matched);
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
        matched(at);
        points -= at as i64;
        previous = Some(at);
        next = at + 1;
    }
    Some(points)
}

/// Map folded character positions back to complete graphemes in the rendered line.
fn highlight(query: &str, line: &str) -> slint::ModelRc<crate::PalettePart> {
    use unicode_segmentation::UnicodeSegmentation;
    let query = query.trim();
    if query.is_empty() || line.is_empty() {
        return Default::default();
    }
    let graphemes: Vec<_> = line.graphemes(true).collect();
    let owners: Vec<_> = graphemes
        .iter()
        .enumerate()
        .flat_map(|(index, part)| std::iter::repeat_n(index, fold(part).chars().count()))
        .collect();
    let mut marks = vec![false; graphemes.len()];
    let mut mark = |index| marks[owners[index]] = true;
    let folded_line = fold(line);
    let folded_query = fold(query);
    let found = if let Some(at) = folded_line.find(&folded_query) {
        let start = folded_line[..at].chars().count();
        (start..start + folded_query.chars().count()).for_each(mark);
        true
    } else {
        score_match(query, line, &mut mark).is_some()
    };
    if !found {
        return Default::default();
    }
    let mut parts = Vec::new();
    let mut start = 0;
    while start < graphemes.len() {
        let end = marks[start..]
            .iter()
            .position(|value| *value != marks[start])
            .map_or(graphemes.len(), |at| start + at);
        parts.push(crate::PalettePart {
            text: graphemes[start..end].concat().into(),
            matched: marks[start],
        });
        start = end;
    }
    slint::ModelRc::new(slint::VecModel::from(parts))
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
            // 書き手の決定 2026-11-03: **字を打って探すのは、開いているフォルダの中だけ。**
            // 空の欄では、外のフォルダの最近のファイルも出す（Explorerのときだけある）。
            if !file.inside {
                return None;
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
                assign: SharedString::new(),
                ..Default::default()
            }
        })
        .collect();
    (
        rows,
        found.into_iter().map(|(_, at)| Shown::File(at)).collect(),
    )
}

/// RFN01-114: いま実行できる操作。アクションの表の行（Quick Draftの中の操作を除く）と、
/// 書き手のデータの行。**前提は1つの答え**（`menu_commands::allowed_for`）。
fn operations_now(
    window: &AppWindow,
    live: &Live,
    pane: PaneId,
    tree: &menu_commands::Target,
) -> Vec<Operation> {
    let tree_scene = menu_commands::Scene::read(window, live, tree);
    let mut found: Vec<Operation> = crate::actions::all()
        .iter()
        .enumerate()
        .filter_map(|(at, action)| {
            let command = action.command.clone()?;
            let enabled = if matches!(command, menu_commands::Command::Tree(_) | menu_commands::Command::PanelAction(_)) {
                (!matches!(command, menu_commands::Command::PanelAction(_)) || !crate::panel_actions::overlay_active(window))
                    && tree_scene.enabled(&command)
            } else {
                menu_commands::allowed_for(window, live, pane, &command) == menu_commands::Allowed::Yes
            };
            enabled.then(|| Operation {
                    category: action.category,
                    title: action.title.clone(),
                    description: action.description.clone(),
                    key: shortcuts::label(window, at),
                    command,
                    action: Some(at),
                })
        })
        .collect();
    found.extend(
        data_operations(window, live, None)
            .into_iter()
            .filter(|operation| {
                menu_commands::allowed_for(window, live, pane, &operation.command)
                    == menu_commands::Allowed::Yes
            }),
    );
    found
}

/// 書き手のデータの行：最近のフォルダ・単語チェックのモード・シェル（テンプレートは別に並べる）。
/// `only`なら、その一覧だけ。
fn data_operations(window: &AppWindow, live: &Live, only: Option<List>) -> Vec<Operation> {
    let wants = |list| only.is_none() || only == Some(list);
    let mut found = Vec::new();
    if wants(List::Recent) {
        for path in crate::offered_folders(live) {
            let name = path.display().to_string();
            found.push(Operation {
                category: crate::actions::Category::File,
                title: (
                    format!("最近のフォルダ：{name}"),
                    format!("Recent Folder: {name}"),
                ),
                description: (
                    "このフォルダを作業フォルダにします。".to_owned(),
                    "Makes this folder the work folder.".to_owned(),
                ),
                key: String::new(),
                command: menu_commands::Command::Recent(path),
                action: None,
            });
        }
    }
    if wants(List::WordModes) {
        for mode in crate::word_modes_now() {
            found.push(Operation {
                category: crate::actions::Category::Edit,
                title: (
                    format!("単語チェック：{}", mode.name),
                    format!("Word Check: {}", mode.name),
                ),
                description: (
                    "いまの文書の単語チェックを、このモードにします。".to_owned(),
                    "Uses this word check mode for the current document.".to_owned(),
                ),
                key: String::new(),
                command: menu_commands::Command::Word(mode.id),
                action: None,
            });
        }
    }
    if wants(List::Shells) {
        for (at, shell) in crate::configured_shells(window).iter().enumerate() {
            found.push(Operation {
                category: crate::actions::Category::File,
                title: (
                    format!("新しいTerminal：{}", shell.name),
                    format!("New Terminal: {}", shell.name),
                ),
                description: (
                    "このシェルで、新しいTerminalのTABを開きます。".to_owned(),
                    "Opens a new Terminal tab with this shell.".to_owned(),
                ),
                key: String::new(),
                command: menu_commands::Command::NewTerminal(at as i32),
                action: None,
            });
        }
    }
    found
}

/// 「Title（Description）」を、上の段に日本語・下の段に英語で（書き手の求め 2026-11-05：説明を
/// 出し、英語でも探せるように）。英語の画面では英語の1段だけ（`two_lines`）。探すのはTitleの両方の
/// 言葉と、別名。
fn ranked_operations(operations: &[Operation], query: &str, categorized: bool) -> Vec<RankedRow> {
    let mut found = Vec::new();
    for (at, operation) in operations.iter().enumerate() {
        let (ja, en) = (&operation.title.0, &operation.title.1);
        let (category_ja, category_en) = operation.category.title();
        let (title_ja, title_en) = if categorized {
            (
                format!("{category_ja}: {ja}"),
                format!("{category_en}: {en}"),
            )
        } else {
            (ja.clone(), en.clone())
        };
        let points = best_score(query, ja, en)
            .into_iter()
            .chain(best_score(query, &title_ja, &title_en))
            .max()
            .or_else(|| alias_score(query, operation.action));
        let Some(points) = points else {
            continue;
        };
        let japanese = format!(
            "{title_ja}（{}）",
            operation.description.0.trim_end_matches('。')
        );
        let english = format!(
            "{title_en} ({})",
            operation.description.1.trim_end_matches('.')
        );
        let (upper, lower) = if crate::i18n::japanese() {
            two_lines(&japanese, &english)
        } else {
            two_lines(&english, &japanese)
        };
        found.push((
            points,
            operation.category.index(),
            Shown::Operation(at),
            PaletteRow {
                title: upper.into(),
                detail: lower.into(),
                key: operation.key.clone().into(),
                assign: operation
                    .action
                    .map(|at| crate::actions::all()[at].id.clone())
                    .unwrap_or_default()
                    .into(),
                ..Default::default()
            },
        ));
    }
    found
}

type RankedRow = (i64, i32, Shown, PaletteRow);

fn best_score(query: &str, ja: &str, en: &str) -> Option<i64> {
    score(query, ja).into_iter().chain(score(query, en)).max()
}

fn finish_ranked(mut rows: Vec<RankedRow>) -> (Vec<PaletteRow>, Vec<Shown>) {
    rows.sort_by_key(|(points, category, _, _)| (std::cmp::Reverse(*points), *category));
    rows.truncate(ROWS_SHOWN);
    rows.into_iter()
        .map(|(_, _, shown, row)| (row, shown))
        .unzip()
}

/// RFN01-107（書き手の求め 2026-11-05）: **名前に無い言葉でも引けるように。**「Terminalを
/// 閉じる」「Editorを閉じる」で「TABを閉じる」を、「Panelを閉じる」で「下段のTABを閉じる」を。
/// 表示名に依存しないように、アクションIDに検索語を対応させる。
const ALIASES: &[(&str, &[&str])] = &[
    (
        "file.close-tab",
        &[
            "Terminalを閉じる",
            "Editorを閉じる",
            "Close Terminal",
            "Close Editor",
        ],
    ),
    (
        "view.close-strip-tab",
        &[
            "Panelを閉じる",
            "Editor Panelを閉じる",
            "Terminal Panelを閉じる",
            "Close Panel",
            "Close Editor Panel",
            "Close Terminal Panel",
        ],
    ),
    ("run.compare-file", &["diff"]),
    ("run.compare-saved", &["diff saved"]),
    ("run.compare-head", &["diff git"]),
];

fn alias_score(query: &str, action: Option<usize>) -> Option<i64> {
    let id = &crate::actions::all()[action?].id;
    ALIASES
        .iter()
        .filter(|(key, _)| *key == id)
        .flat_map(|(_, words)| words.iter())
        .filter_map(|word| score(query, word))
        .max()
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

    #[test]
    fn palette_diagnostics_record_stages_without_the_query() {
        let (harness, _) = crate::saving::Harness::new(|weak| OpenDocument::untitled(1, weak));
        let window = &harness.window;
        let live = &harness.live;
        let directory = app_data::app_directory().unwrap().join("palette-diagnostics");
        let config = crate::diag::Config::from_args(["--diagnostics=ui".into()]).unwrap();
        let path = {
            let mut cache = live.cache.borrow_mut();
            cache.diag.start_in(&directory, config);
            cache.diag.path().unwrap().to_owned()
        };
        window.set_custom_title(true);
        install(window, live);
        window.show().unwrap();
        window.invoke_palette_requested(true);
        slint::platform::update_timers_and_animations();
        let query = "PRIVATE_PALETTE_QUERY";
        window.invoke_palette_edited(format!("> {query}").into());
        let written = std::fs::read_to_string(path).unwrap();
        close(window);
        let mut after = written.as_str();
        for stage in [
            "stage=open-begin",
            "stage=initial-rows-begin",
            "stage=initial-rows-published",
            "stage=focus-request",
            "stage=field-focus-begin",
            "stage=field-focus-returned",
            "stage=filter-begin",
            "stage=publish-returned",
        ] {
            after = after.split_once(stage).unwrap_or_else(|| panic!("missing {stage}: {written}")).1;
        }
        assert!(!written.contains(query));
    }

    #[test]
    fn diff_search_lists_file_comparison_first() {
        let (harness, _) = crate::saving::Harness::new(|weak| OpenDocument::untitled(1, weak));
        let window = &harness.window;
        install(window, &harness.live);
        window.invoke_palette_requested(true);
        window.invoke_palette_edited("> diff".into());

        let rows = window.get_palette_rows();
        assert_eq!(rows.row_data(0).unwrap().assign.as_str(), "run.compare-file");
        close(window);
    }

    /// RFN01-87: the letters in order, together counting for more.
    #[test]
    fn highlighting_preserves_visible_text_and_marks_only_the_match() {
        let parts = |query, text| {
            highlight(query, text)
                .iter()
                .map(|part| (part.text.to_string(), part.matched))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            parts("ＳＡＶＥ ａｓ", "File: Save As…"),
            [
                ("File: ".into(), false),
                ("Save As".into(), true),
                ("…".into(), false),
            ]
        );
        assert_eq!(
            parts("fm", "file.md"),
            [
                ("f".into(), true),
                ("ile.".into(), false),
                ("m".into(), true),
                ("d".into(), false),
            ]
        );
        assert_eq!(
            parts("か", "か\u{3099}き"),
            [("か\u{3099}".into(), true), ("き".into(), false),]
        );
        assert!(parts("xyz", "File: Save As…").is_empty());
    }

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
            inside: true,
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
        // A recent file outside the folder: listed while nothing is typed, not
        // found by a search.
        let mut files = files;
        files.insert(
            0,
            FileCandidate {
                inside: false,
                ..file("outside-notes.md", true)
            },
        );
        let (rows, _) = file_rows(&files, "");
        assert_eq!(rows[0].title, "outside-notes.md");
        let (rows, _) = file_rows(&files, "notes");
        assert!(rows.iter().all(|row| row.title != "outside-notes.md"));
    }
}
