//! Repository inventory backed by the Workspace registry.
use super::*;
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Show,
    Refresh,
    Open,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Member {
    workspace: workspace::WorkspaceId,
    workspace_name: String,
    folder: workspace::FolderId,
    path: PathBuf,
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    branch: String,
    error: Option<String>,
    members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    member: Member,
    root: PathBuf,
}

#[derive(Default)]
struct State {
    scan: Option<Receiver<Vec<Entry>>>,
    entries: Vec<Entry>,
    visible: Vec<usize>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
    static POLL: Timer = Timer::default();
}

fn registrations(live: &Live) -> Option<Vec<Member>> {
    let runtime = live.folder.borrow().workspace.clone()?;
    let runtime = runtime.borrow();
    if runtime.read_error().is_some() {
        return None;
    }
    let registry = runtime.registry();
    Some(
        registry
            .workspaces()
            .iter()
            .flat_map(|item| {
                item.folders.iter().filter_map(|id| {
                    let folder = registry.folder(*id)?;
                    Some(Member {
                        workspace: item.id,
                        workspace_name: item.name.clone(),
                        folder: *id,
                        path: crate::backup::plain(&folder.path),
                    })
                })
            })
            .collect(),
    )
}

fn inspect(members: Vec<Member>) -> Vec<Entry> {
    let mut known_folders = BTreeMap::new();
    let mut entries = BTreeMap::new();
    for member in members {
        let result = known_folders.entry(member.folder).or_insert_with(|| {
            git::registered_repository(&member.path).map_err(|error| error.to_string())
        });
        let (path, error) = match result {
            Ok(Some(root)) => (root.clone(), None),
            Ok(None) => continue,
            Err(error) => (member.path.clone(), Some(error.clone())),
        };
        let entry = entries
            .entry((error.is_some(), path.clone()))
            .or_insert_with(|| {
                let branch = if error.is_some() {
                    String::new()
                } else {
                    git::current_branch(&path).unwrap_or_else(|| {
                        git::head_sha(&path).map_or_else(
                            || pick("ブランチなし", "No branch").to_owned(),
                            |sha| {
                                let short = sha.chars().take(8).collect::<String>();
                                say!("分離HEAD · {short}", "Detached HEAD · {short}")
                            },
                        )
                    })
                };
                Entry {
                    path,
                    branch,
                    error,
                    members: Vec::new(),
                }
            });
        entry.members.push(member);
    }
    let mut entries: Vec<_> = entries.into_values().collect();
    entries.sort_by_cached_key(|entry| {
        (
            entry.error.is_some(),
            name(&entry.path).to_lowercase(),
            entry.path.clone(),
        )
    });
    entries
}

fn name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

impl Entry {
    fn destination(&self, active: Option<workspace::WorkspaceId>) -> &Member {
        self.members
            .iter()
            .find(|member| Some(member.workspace) == active)
            .unwrap_or(&self.members[0])
    }

    fn workspace_names(&self) -> String {
        let mut seen = std::collections::BTreeSet::new();
        self.members
            .iter()
            .filter_map(|member| {
                seen.insert(member.workspace)
                    .then_some(member.workspace_name.as_str())
            })
            .collect::<Vec<_>>()
            .join("  ·  ")
    }
}

fn active_workspace(live: &Live) -> Option<workspace::WorkspaceId> {
    live.folder
        .borrow()
        .workspace
        .as_ref()?
        .borrow()
        .active_workspace()
}

pub(crate) fn selection(window: &AppWindow, live: &Live) -> Option<Selection> {
    if !window.get_git_catalog_open() {
        return None;
    }
    let active = active_workspace(live);
    STATE.with(|state| {
        let state = state.borrow();
        let index = usize::try_from(window.get_git_catalog_selected()).ok()?;
        let entry = state.entries.get(*state.visible.get(index)?)?;
        entry.error.is_none().then(|| Selection {
            member: entry.destination(active).clone(),
            root: entry.path.clone(),
        })
    })
}

fn publish(window: &AppWindow, live: &Live) {
    let active = active_workspace(live);
    let chosen = git_ui::chosen_folder();
    let terms = window.get_git_catalog_filter().to_lowercase();
    let current_only = window.get_git_catalog_current_only() && active.is_some();
    let (rows, selected, scanning) = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let previous = usize::try_from(window.get_git_catalog_selected())
            .ok()
            .and_then(|index| state.visible.get(index).copied());
        let mut visible = Vec::new();
        let mut rows = Vec::new();
        for (index, entry) in state.entries.iter().enumerate() {
            if current_only
                && !entry
                    .members
                    .iter()
                    .any(|member| Some(member.workspace) == active)
            {
                continue;
            }
            let workspaces = entry.workspace_names();
            let path = entry.path.to_string_lossy();
            let searchable = format!("{path} {} {workspaces}", entry.branch).to_lowercase();
            if !terms
                .split_whitespace()
                .all(|term| searchable.contains(term))
            {
                continue;
            }
            let destination = entry.destination(active);
            let current = entry.error.is_none()
                && entry.members.iter().any(|member| {
                    Some(member.workspace) == active
                        && chosen
                            .as_ref()
                            .is_some_and(|path| git_ui::same_path(path, &member.path))
                });
            visible.push(index);
            rows.push(GitCatalogRow {
                name: name(&entry.path).into(),
                path: path.as_ref().into(),
                branch: entry.branch.as_str().into(),
                workspaces: workspaces.into(),
                destination: if Some(destination.workspace) != active {
                    destination.workspace_name.as_str().into()
                } else {
                    "".into()
                },
                error: entry.error.as_deref().unwrap_or_default().into(),
                current,
            });
        }
        let selected = previous
            .and_then(|index| visible.iter().position(|visible| *visible == index))
            .or_else(|| rows.iter().position(|row| row.current))
            .or_else(|| rows.iter().position(|row| row.error.is_empty()))
            .or_else(|| (!rows.is_empty()).then_some(0));
        state.visible = visible;
        (rows, selected, state.scan.is_some())
    });
    window.set_git_catalog_rows(ModelRc::new(VecModel::from(rows)));
    window.set_git_catalog_selected(selected.map_or(-1, |index| index as i32));
    window.set_git_catalog_scanning(scanning);
}

fn poll(window: &AppWindow, live: &Live) {
    let changed = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(scan) = &state.scan else {
            return false;
        };
        let entries = match scan.try_recv() {
            Ok(entries) => entries,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                window.set_git_catalog_note(
                    pick(
                        "一覧を読み込めませんでした。",
                        "Could not read repositories.",
                    )
                    .into(),
                );
                Vec::new()
            }
        };
        state.scan = None;
        state.entries = entries;
        true
    });
    if changed {
        POLL.with(Timer::stop);
        publish(window, live);
    }
}

fn refresh(window: &AppWindow, live: &Live) {
    window.set_git_catalog_note("".into());
    STATE.with(|state| *state.borrow_mut() = State::default());
    let Some(members) = registrations(live) else {
        window.set_git_catalog_note(
            pick(
                "Workspaceの登録を読み込めません。",
                "Could not read the Workspace registry.",
            )
            .into(),
        );
        publish(window, live);
        return;
    };
    let (sender, receiver) = mpsc::channel();
    STATE.with(|state| state.borrow_mut().scan = Some(receiver));
    std::thread::spawn(move || {
        let _ = sender.send(inspect(members));
    });
    publish(window, live);
    let (weak, live) = (window.as_weak(), live.clone());
    POLL.with(|timer| {
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                if let Some(window) = weak.upgrade() {
                    poll(&window, &live);
                }
            },
        )
    });
}

pub(crate) fn regain_focus(window: &AppWindow) {
    let weak = window.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        if let Some(window) = weak.upgrade()
            && window.get_git_catalog_open()
            && !window.get_question_open()
            && !window.get_palette_open()
        {
            window.set_git_catalog_focus(window.get_git_catalog_focus() + 1);
        }
    });
}

fn close(window: &AppWindow) {
    POLL.with(Timer::stop);
    STATE.with(|state| *state.borrow_mut() = State::default());
    window.set_git_catalog_open(false);
    window.set_git_catalog_rows(ModelRc::default());
    restore_editor_focus(window);
}

fn valid_registration(live: &Live, selection: &Selection) -> bool {
    let Some(runtime) = live.folder.borrow().workspace.clone() else {
        return false;
    };
    let member = &selection.member;
    let registered = {
        let runtime = runtime.borrow();
        let registry = runtime.registry();
        registry
            .workspace(member.workspace)
            .is_some_and(|workspace| workspace.folders.contains(&member.folder))
            && registry
                .folder(member.folder)
                .is_some_and(|folder| git_ui::same_path(&folder.path, &member.path))
    };
    registered
        && matches!(git::registered_repository(&selection.member.path),
            Ok(Some(root)) if git_ui::same_path(&root, &selection.root))
}

fn changed(window: &AppWindow, live: &Live) {
    refresh(window, live);
    window.set_git_catalog_note(
        pick(
            "登録またはリポジトリの状態が変わりました。選び直してください。",
            "The registration or repository changed. Select it again.",
        )
        .into(),
    );
    regain_focus(window);
}

fn open(window: &AppWindow, live: &Live, selection: Selection) {
    let workspace = Some(selection.member.workspace);
    if active_workspace(live) == workspace {
        open_after_switch(window, live, selection);
        return;
    }
    if !valid_registration(live, &selection) {
        changed(window, live);
        return;
    }
    request_workspace_change_with_cleanup(
        window,
        live,
        workspace,
        Some(WorkspaceCleanup::OpenRepository(selection)),
    );
}

pub(crate) fn check_workspace_destination(window: &AppWindow, live: &Live) -> bool {
    let valid = match &live.folder.borrow().pending_workspace_cleanup {
        Some(WorkspaceCleanup::OpenRepository(selection)) => valid_registration(live, selection),
        _ => true,
    };
    if !valid {
        live.folder.borrow_mut().pending_workspace_cleanup = None;
        changed(window, live);
    }
    valid
}

pub(crate) fn open_after_switch(window: &AppWindow, live: &Live, selection: Selection) {
    if !window.get_git_catalog_open() {
        return;
    }
    if active_workspace(live) != Some(selection.member.workspace)
        || !valid_registration(live, &selection)
    {
        changed(window, live);
        return;
    }
    close(window);
    window.set_tree_open(true);
    window.set_left_tab(LeftTab::Git.index());
    git_ui::select_registered_folder(window, live, &selection.member.path);
}

pub(crate) fn execute(window: &AppWindow, live: &Live, action: Action, target: Option<&Selection>) {
    match action {
        Action::Show => {
            window.set_git_catalog_filter("".into());
            window.set_git_catalog_current_only(false);
            window.set_git_catalog_open(true);
            refresh(window, live);
            regain_focus(window);
        }
        Action::Refresh => refresh(window, live),
        Action::Close => close(window),
        Action::Open => {
            if let Some(target) = target {
                open(window, live, target.clone());
            }
        }
    }
}

pub(crate) fn wire(window: &AppWindow, live: &Live) {
    let (weak, live) = (window.as_weak(), live.clone());
    window.on_git_catalog_filter_changed(move || {
        if let Some(window) = weak.upgrade() {
            publish(&window, &live);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{git_ok, repository};
    use crate::saving::{Harness, attach_workspace, edit, open_under, scratch_directory};
    use slint::Model;
    use std::fs;

    #[test]
    fn catalog_deduplicates_worktrees_and_uses_the_workspace_transition() {
        let parent = scratch_directory("git-catalog");
        let source = parent.join("original").join("notes");
        let clone = parent.join("copies").join("notes");
        let subfolder = source.join("chapter");
        let ordinary = parent.join("ordinary");
        let unavailable = parent.join("unavailable");
        for directory in [&subfolder, &clone, &ordinary, &unavailable] {
            fs::create_dir_all(directory).unwrap();
        }
        if repository(&source).is_none() {
            return;
        }
        let (harness, document) =
            Harness::new(|weak| open_under(&source, "note.md", "base\n", weak));
        git::commit(&source, "base", true, false).unwrap();
        git_ok(&clone, &["clone", "--quiet", source.to_str().unwrap(), "."]);
        git_ok(&clone, &["switch", "-c", "draft"]);
        let (window, live) = (&harness.window, &harness.live);
        attach_workspace(live, &source, workspace::SaveMode::Recovery);
        let source_workspace = active_workspace(live).unwrap();
        let runtime = live.folder.borrow().workspace.clone().unwrap();
        let other = runtime
            .borrow_mut()
            .edit(|registry| {
                let id = registry.create_workspace("Archive".into())?;
                for directory in [&subfolder, &clone, &ordinary, &unavailable] {
                    registry.add_root(id, directory)?;
                }
                Ok(id)
            })
            .unwrap();
        fs::rename(&unavailable, parent.join("moved")).unwrap();
        window.set_workspace_active(true);
        git_ui::publish(window, live);
        let run = |action| {
            let target = menu_commands::Target::for_surface(window, live);
            menu_commands::run(
                window,
                live,
                &target,
                menu_commands::Command::PanelAction(panel_actions::Action::Repositories(action)),
                "",
            );
        };
        run(Action::Show);
        for _ in 0..500 {
            poll(window, live);
            if !window.get_git_catalog_scanning() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let rows = window.get_git_catalog_rows();
        assert_eq!(rows.row_count(), 3);
        assert_eq!(rows.row_data(0).unwrap().name, "notes");
        assert_eq!(rows.row_data(1).unwrap().name, "notes");
        assert!(!rows.row_data(2).unwrap().error.is_empty());
        assert!(rows.row_data(1).unwrap().current);
        assert!(rows.row_data(1).unwrap().workspaces.contains("Archive"));
        assert_eq!(rows.row_data(0).unwrap().branch, "draft");
        window.set_git_catalog_current_only(true);
        publish(window, live);
        assert_eq!(window.get_git_catalog_rows().row_count(), 1);
        window.set_git_catalog_current_only(false);
        window.set_git_catalog_filter("draft".into());
        publish(window, live);
        assert_eq!(window.get_git_catalog_rows().row_count(), 1);
        assert_eq!(
            window
                .get_git_catalog_rows()
                .row_data(0)
                .unwrap()
                .destination,
            "Archive"
        );

        edit(&document, "unsaved\n");
        run(Action::Open);
        assert!(
            matches!(*live.pending.borrow(), Some(Question::ChangeWorkspace(Some(id))) if id == other)
        );
        answer_question(window, live, 2);
        assert_eq!(active_workspace(live), Some(source_workspace));
        assert!(document.text.edited());
        assert!(window.get_git_catalog_open());
        assert!(!window.get_git_repo_active());
        run(Action::Open);
        answer_question(window, live, 0);
        assert_eq!(active_workspace(live), Some(other));
        assert!(!window.get_git_catalog_open());
        assert!(window.get_git_repo_active());
        assert!(git_repo_ui::root().is_some_and(|path| git_ui::same_path(&path, &clone)));
        assert!(git_ui::chosen_folder().is_some_and(|path| git_ui::same_path(&path, &clone)));
        assert_eq!(
            fs::read_to_string(source.join("note.md")).unwrap(),
            "base\nunsaved\n"
        );
    }
}
