//! Repository inventory backed by the Workspace registry.
use super::*;
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Enter,
    Show,
    Refresh,
    Open,
    Unregister,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Member {
    workspace: Option<workspace::WorkspaceId>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogTarget {
    open: Vec<Selection>,
    registrations: Vec<Member>,
}

impl CatalogTarget {
    pub(crate) fn can_open(&self) -> bool {
        !self.open.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Removal {
    member: Member,
    remove_folder: bool,
}

impl Selection {
    pub(crate) fn workspace_name(&self) -> &str {
        &self.member.workspace_name
    }
}

#[derive(Default)]
struct State {
    scan: Option<Receiver<Vec<Entry>>>,
    auto_open: Option<workspace::WorkspaceId>,
    return_to_changes: bool,
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
    let mut members = Vec::new();
    let mut assigned = std::collections::BTreeSet::new();
    for item in registry.workspaces() {
        for folder in item.folders.iter().filter_map(|id| registry.folder(*id)) {
            assigned.insert(folder.id);
            members.push(Member {
                workspace: Some(item.id),
                workspace_name: item.name.clone(),
                folder: folder.id,
                path: crate::backup::plain(&folder.path),
            });
        }
    }
    for folder in registry
        .folders()
        .iter()
        .filter(|folder| !assigned.contains(&folder.id))
    {
        members.push(Member {
            workspace: None,
            workspace_name: pick("所属Workspaceなし", "No Workspace").to_owned(),
            folder: folder.id,
            path: crate::backup::plain(&folder.path),
        });
    }
    Some(members)
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

fn path_label(path: &Path) -> String {
    let mut components: Vec<_> = path.components().rev().take(3).collect();
    components.reverse();
    let tail: PathBuf = components.into_iter().collect();
    if tail == path {
        path.to_string_lossy().into_owned()
    } else {
        format!("…\\{}", tail.display())
    }
}

impl Entry {
    fn member_in(&self, active: Option<workspace::WorkspaceId>) -> Option<&Member> {
        let active = active?;
        self.members
            .iter()
            .find(|member| member.workspace == Some(active))
    }

    fn destinations(&self, active: Option<workspace::WorkspaceId>) -> Vec<Selection> {
        let mut seen = std::collections::BTreeSet::new();
        let current = self.member_in(active);
        self.members
            .iter()
            .filter(|member| member.workspace.is_some())
            .filter(|member| current.is_none_or(|current| member.workspace == current.workspace))
            .filter(|member| seen.insert(member.workspace))
            .map(|member| Selection {
                member: member.clone(),
                root: self.path.clone(),
            })
            .collect()
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

pub(crate) fn git_pane_visible(window: &AppWindow) -> bool {
    window.get_tree_open() && window.get_left_tab() == LeftTab::Git.index()
}

pub(crate) fn visible(window: &AppWindow) -> bool {
    window.get_git_catalog_open() && git_pane_visible(window)
}

pub(crate) fn selection(window: &AppWindow, live: &Live) -> Option<CatalogTarget> {
    if !visible(window) {
        return None;
    }
    let active = active_workspace(live);
    STATE.with(|state| {
        let state = state.borrow();
        let index = usize::try_from(window.get_git_catalog_selected()).ok()?;
        let entry = state.entries.get(*state.visible.get(index)?)?;
        Some(CatalogTarget {
            open: if entry.error.is_none() {
                entry.destinations(active)
            } else {
                Vec::new()
            },
            registrations: entry.members.clone(),
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
            let in_workspace = entry.member_in(active).is_some();
            if current_only && !in_workspace {
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
            let current = entry.error.is_none()
                && entry.members.iter().any(|member| {
                    in_workspace
                        && member.workspace == active
                        && chosen
                            .as_ref()
                            .is_some_and(|path| git_ui::same_path(path, &member.path))
                });
            visible.push(index);
            let openable = entry.error.is_none()
                && entry
                    .members
                    .iter()
                    .any(|member| member.workspace.is_some());
            rows.push(GitCatalogRow {
                name: name(&entry.path).into(),
                path: path.as_ref().into(),
                path_label: path_label(&entry.path).into(),
                branch: entry.branch.as_str().into(),
                workspaces: workspaces.into(),
                switch_workspace: active.is_some() && !in_workspace && openable,
                openable,
                error: entry.error.as_deref().unwrap_or_default().into(),
                current,
            });
        }
        let selected = previous
            .and_then(|index| visible.iter().position(|visible| *visible == index))
            .or_else(|| rows.iter().position(|row| row.current))
            .or_else(|| {
                rows.iter()
                    .position(|row| !row.switch_workspace && row.openable)
            })
            .or_else(|| rows.iter().position(|row| row.openable))
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
        let target = STATE.with(|state| {
            let mut state = state.borrow_mut();
            let workspace = state.auto_open.take()?;
            if !visible(window)
                || active_workspace(live) != Some(workspace)
                || !window.get_git_catalog_filter().is_empty()
                || window.get_git_catalog_current_only()
                || window.get_palette_open()
                || panel_actions::overlay_active(window)
            {
                return None;
            }
            let mut entries = state.entries.iter().filter(|entry| {
                entry.error.is_none() && entry.member_in(Some(workspace)).is_some()
            });
            let entry = entries.next()?;
            if entries.next().is_some() {
                return None;
            }
            entry.destinations(Some(workspace)).pop()
        });
        if let Some(target) = target {
            if window.get_git_busy() || live.pending.borrow().is_some() {
                return;
            }
            open_after_switch(window, live, target);
        }
    }
}

fn refresh(window: &AppWindow, live: &Live) {
    window.set_git_catalog_note("".into());
    POLL.with(Timer::stop);
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.scan = None;
        state.entries.clear();
        state.visible.clear();
    });
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
            && visible(&window)
            && !window.get_question_open()
            && !window.get_palette_open()
            && !panel_actions::overlay_active(&window)
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
    let Some(workspace) = member.workspace else {
        return false;
    };
    let registered = {
        let runtime = runtime.borrow();
        let registry = runtime.registry();
        registry
            .workspace(workspace)
            .is_some_and(|workspace| workspace.folders.contains(&member.folder))
            && registry
                .folder(member.folder)
                .is_some_and(|folder| git_ui::same_path(&folder.path, &member.path))
    };
    registered
        && matches!(git::registered_repository(&selection.member.path),
            Ok(Some(root)) if git_ui::same_path(&root, &selection.root))
}

impl Removal {
    fn read(registry: &workspace::Registry, member: &Member) -> Option<Self> {
        let folder = registry.folder(member.folder)?;
        if !git_ui::same_path(&folder.path, &member.path) {
            return None;
        }
        let users = registry.folder_users(member.folder);
        match member.workspace {
            Some(id)
                if users.contains(&id) && registry.workspace(id)?.name == member.workspace_name => {
            }
            None if users.is_empty() => {}
            _ => return None,
        }
        Some(Self {
            member: member.clone(),
            remove_folder: users.len() <= 1,
        })
    }
}

fn choose_removal(window: &AppWindow, live: &Live, members: &[Member]) {
    if let [member] = members {
        confirm_removal(window, live, member);
    } else if !members.is_empty() {
        let labels: Vec<_> = members
            .iter()
            .map(|member| format!("{} · {}", member.workspace_name, path_label(&member.path)))
            .collect();
        let mut choices: Vec<_> = labels.iter().map(String::as_str).collect();
        choices.push(cancel());
        ask_question(
            window,
            live,
            Question::ChooseRepositoryRegistration(members.to_vec()),
            pick(
                "解除する登録を選んでください。",
                "Choose the registration to remove.",
            )
            .into(),
            &choices,
            -1,
        );
    }
}

fn can_remove_from_active(
    window: &AppWindow,
    live: &Live,
    runtime: &workspace_ui::Runtime,
    member: &Member,
) -> bool {
    let Some(id) = member
        .workspace
        .filter(|id| runtime.active_workspace() == Some(*id))
    else {
        return true;
    };
    let registry = runtime.registry();
    let remaining: Vec<_> = registry
        .workspace(id)
        .into_iter()
        .flat_map(|item| item.folders.iter().copied())
        .filter(|folder| *folder != member.folder)
        .filter_map(|folder| registry.folder(folder))
        .map(|folder| &folder.path)
        .collect();
    let holds_open_file = open_documents(live).iter().any(|document| {
        document.file.borrow().path().is_some_and(|path| {
            git_ui::inside(path, &member.path)
                && !remaining.iter().any(|root| git_ui::inside(path, root))
        })
    });
    if holds_open_file {
        window.tell(
            pick(
                "対象フォルダの開いているタブを閉じてから、登録を解除してください。",
                "Close open tabs from this folder before removing its registration.",
            )
            .into(),
        );
    }
    !holds_open_file
}

pub(crate) fn confirm_removal(window: &AppWindow, live: &Live, member: &Member) {
    let Some(runtime) = live.folder.borrow().workspace.clone() else {
        return;
    };
    let removal = {
        let runtime = runtime.borrow();
        if !can_remove_from_active(window, live, &runtime, member) {
            return;
        }
        Removal::read(runtime.registry(), member)
    };
    let Some(removal) = removal else {
        changed(window, live);
        return;
    };
    let path = member.path.display();
    let name = &member.workspace_name;
    let details = match (member.workspace, removal.remove_folder) {
        (None, _) => say!("所属Workspaceなし\n\n{path}", "No Workspace\n\n{path}"),
        (Some(_), true) => say!(
            "Workspace「{name}」の最後の紐づけと、RFN Editのフォルダ登録を解除します。\n\n{path}",
            "Remove the folder registration and its last association, with Workspace “{name}”.\n\n{path}"
        ),
        (Some(_), false) => say!(
            "Workspace「{name}」との紐づけを外します。\n\n{path}",
            "Remove the association with Workspace “{name}”.\n\n{path}"
        ),
    };
    let text = format!(
        "{}\n\n{details}\n\n{}",
        pick("この登録を解除しますか？", "Remove this registration?"),
        pick(
            "RFN Editの登録情報だけが変わります。ディスク上のファイルとGit履歴はそのまま残ります。",
            "Only RFN Edit’s registration changes. Files on disk and Git history stay intact."
        )
    );
    ask_question(
        window,
        live,
        Question::UnregisterRepository(removal),
        text,
        &[pick("登録を解除する", "Remove Registration"), cancel()],
        -1,
    );
}

pub(crate) fn unregister(window: &AppWindow, live: &Live, removal: Removal) {
    let Some(runtime) = live.folder.borrow().workspace.clone() else {
        return;
    };
    let member = &removal.member;
    let mut runtime = runtime.borrow_mut();
    if Removal::read(runtime.registry(), member).as_ref() != Some(&removal) {
        drop(runtime);
        changed(window, live);
        return;
    }
    if window.get_git_busy() || !can_remove_from_active(window, live, &runtime, member) {
        return;
    }
    let active = runtime.active_workspace();
    let result = runtime.edit(|registry| {
        if let Some(workspace) = member.workspace {
            registry.remove_root(workspace, member.folder)?;
        }
        if removal.remove_folder {
            registry.remove_unused_folder(member.folder)?;
        }
        Ok(())
    });
    drop(runtime);
    match result {
        Ok(()) => {
            if let Some(id) = active.filter(|id| member.workspace == Some(*id)) {
                refresh_workspace_tree(window, live, id);
            }
            publish_workspace_manager(window, live);
            refresh(window, live);
            window.tell(pick("登録を解除しました。", "Registration removed.").into());
        }
        Err(error) => window.tell(workspace_edit_error_message(&error).into()),
    }
}

fn changed(window: &AppWindow, live: &Live) {
    window.set_tree_open(true);
    window.set_left_tab(LeftTab::Git.index());
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

pub(crate) fn open(window: &AppWindow, live: &Live, selection: Selection) {
    let workspace = selection.member.workspace;
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

pub(crate) fn workspace_destination(live: &Live) -> Option<String> {
    let folder = live.folder.borrow();
    let Some(WorkspaceCleanup::OpenRepository(selection)) = &folder.pending_workspace_cleanup
    else {
        return None;
    };
    Some(selection.workspace_name().to_owned())
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
    if active_workspace(live) != selection.member.workspace || !valid_registration(live, &selection)
    {
        changed(window, live);
        return;
    }
    close(window);
    window.set_tree_open(true);
    window.set_left_tab(LeftTab::Git.index());
    git_ui::show_changes(window, live, selection.member.path);
}

fn show(window: &AppWindow, live: &Live, auto_open: bool) {
    let return_to_changes = git_pane_visible(window) && !window.get_git_catalog_open();
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.auto_open = if auto_open {
            active_workspace(live)
        } else {
            None
        };
        state.return_to_changes = return_to_changes;
    });
    window.set_git_catalog_filter("".into());
    window.set_git_catalog_current_only(false);
    window.set_tree_open(true);
    window.set_left_tab(LeftTab::Git.index());
    window.set_git_catalog_open(true);
    refresh(window, live);
    regain_focus(window);
}

pub(crate) fn execute(
    window: &AppWindow,
    live: &Live,
    action: Action,
    target: Option<&CatalogTarget>,
) {
    match action {
        Action::Enter => {
            if git_pane_visible(window) {
                close(window);
                window.set_tree_open(false);
            } else {
                show(window, live, true);
            }
        }
        Action::Show => show(window, live, false),
        Action::Refresh => refresh(window, live),
        Action::Close => {
            let active = active_workspace(live);
            let return_to_changes = STATE.with(|state| {
                let state = state.borrow();
                state.return_to_changes
                    || (active.is_some()
                        && state.scan.is_none()
                        && !state.entries.iter().any(|entry| {
                            entry.error.is_none() && entry.member_in(active).is_some()
                        }))
            });
            close(window);
            if return_to_changes {
                git_ui::publish(window, live);
            } else {
                window.set_tree_open(false);
            }
        }
        Action::Open => match target.map(|target| target.open.as_slice()) {
            Some([target]) => open(window, live, target.clone()),
            Some(targets) if targets.len() > 1 => {
                let mut choices: Vec<_> = targets.iter().map(Selection::workspace_name).collect();
                choices.push(cancel());
                ask_question(
                    window,
                    live,
                    Question::ChooseRepositoryWorkspace(targets.to_vec()),
                    pick(
                        "リポジトリを開くWorkspaceを選んでください。",
                        "Choose a Workspace for this repository.",
                    )
                    .to_owned(),
                    &choices,
                    -1,
                );
            }
            _ => {}
        },
        Action::Unregister => {
            if let Some(target) = target {
                choose_removal(window, live, &target.registrations);
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
    fn unregister_uses_the_ledger_for_missing_and_untrusted_folders() {
        let root = scratch_directory("git-catalog-unregister");
        let source = root.join("untrusted");
        let missing = root.join("missing");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&missing).unwrap();
        fs::write(source.join("keep.md"), "leave this alone").unwrap();
        let (harness, document) = Harness::new(|weak| open_under(&source, "note.md", "kept", weak));
        let (window, live) = (&harness.window, &harness.live);
        attach_workspace(live, &source, workspace::SaveMode::Recovery);
        let active = active_workspace(live).unwrap();
        let runtime = live.folder.borrow().workspace.clone().unwrap();
        let (other, shared, unused) = runtime
            .borrow_mut()
            .edit(|registry| {
                let other = registry.create_workspace("Archive".into())?;
                let shared = registry.add_root(other, &source)?;
                let unused = registry.add_root(other, &missing)?;
                registry.remove_root(other, unused)?;
                Ok((other, shared, unused))
            })
            .unwrap();
        fs::remove_dir(&missing).unwrap();
        let members = registrations(live).unwrap();
        let (untrusted, other_members): (Vec<_>, Vec<_>) = members
            .into_iter()
            .partition(|member| member.folder == shared);
        let mut entries = inspect(other_members);
        assert_eq!(
            entries.len(),
            1,
            "the missing registration has no Workspace"
        );
        assert_eq!(entries[0].members[0].workspace, None);
        entries.push(Entry {
            path: source.clone(),
            branch: String::new(),
            error: Some(git::GitError::Untrusted.to_string()),
            members: untrusted,
        });
        window.set_tree_open(true);
        window.set_left_tab(LeftTab::Git.index());
        window.set_git_catalog_open(true);
        STATE.with(|state| state.borrow_mut().entries = entries);
        publish(window, live);

        let command = menu_commands::Command::PanelAction(panel_actions::Action::Repositories(
            Action::Unregister,
        ));
        let run = |target: &menu_commands::Target| {
            menu_commands::run(window, live, target, command.clone(), "");
        };
        let captured = menu_commands::Target::for_surface(window, live);
        window.set_git_catalog_selected(1);
        run(&captured);
        assert!(live.pending.borrow().is_none());
        run(&menu_commands::Target::for_surface(window, live));
        let Some(Question::ChooseRepositoryRegistration(members)) = live.pending.borrow().clone()
        else {
            panic!("shared folders need a registration choice");
        };
        let active_choice = members
            .iter()
            .position(|member| member.workspace == Some(active))
            .unwrap();
        answer_question(window, live, active_choice as i32);
        assert!(
            live.pending.borrow().is_none(),
            "do not close or save a tab as part of unregistering"
        );
        assert_eq!(runtime.borrow().registry().folder_users(shared).len(), 2);
        run(&menu_commands::Target::for_surface(window, live));
        let choice = members
            .iter()
            .position(|member| member.workspace == Some(other))
            .unwrap();
        answer_question(window, live, choice as i32);
        assert!(matches!(
            *live.pending.borrow(),
            Some(Question::UnregisterRepository(_))
        ));
        assert!(window.get_question_detail().contains("Git"));
        answer_question(window, live, 1);
        assert_eq!(runtime.borrow().registry().folder_users(shared).len(), 2);
        run(&menu_commands::Target::for_surface(window, live));
        answer_question(window, live, choice as i32);
        answer_question(window, live, 0);
        assert_eq!(
            runtime.borrow().registry().folder_users(shared),
            vec![active]
        );
        assert_eq!(
            fs::read_to_string(source.join("keep.md")).unwrap(),
            "leave this alone"
        );
        assert_eq!(active_workspace(live), Some(active));
        assert_eq!(document.text.borrow().as_str(), "kept");

        for _ in 0..500 {
            poll(window, live);
            if !window.get_git_catalog_scanning() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        window.set_git_catalog_filter("missing".into());
        publish(window, live);
        run(&menu_commands::Target::for_surface(window, live));
        assert!(matches!(
            *live.pending.borrow(),
            Some(Question::UnregisterRepository(_))
        ));
        answer_question(window, live, 0);
        assert!(runtime.borrow().registry().folder(unused).is_none());
        assert!(!missing.exists());
        assert!(visible(window));
    }

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
        let wait_for_catalog = || {
            for _ in 0..500 {
                poll(window, live);
                if !window.get_git_catalog_scanning() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("repository scan did not finish");
        };
        run(Action::Enter);
        wait_for_catalog();
        assert!(!visible(window));
        assert_eq!(window.get_left_tab(), LeftTab::Git.index());
        run(Action::Show);
        wait_for_catalog();
        assert!(visible(window));
        let rows = window.get_git_catalog_rows();
        assert_eq!(rows.row_count(), 3);
        assert_eq!(rows.row_data(0).unwrap().name, "notes");
        assert_eq!(rows.row_data(1).unwrap().name, "notes");
        assert!(!rows.row_data(2).unwrap().error.is_empty());
        assert!(rows.row_data(1).unwrap().current);
        assert!(rows.row_data(1).unwrap().workspaces.contains("Archive"));
        assert_eq!(rows.row_data(0).unwrap().branch, "draft");
        assert_ne!(
            rows.row_data(0).unwrap().path_label,
            rows.row_data(1).unwrap().path_label
        );
        window.set_git_catalog_current_only(true);
        publish(window, live);
        assert_eq!(window.get_git_catalog_rows().row_count(), 1);
        window.set_git_catalog_current_only(false);
        publish(window, live);
        let command =
            menu_commands::Command::PanelAction(panel_actions::Action::Repositories(Action::Open));
        window.set_git_catalog_selected(1);
        let palette_target = menu_commands::Target::for_surface(window, live);
        window.set_git_catalog_filter("draft".into());
        publish(window, live);
        assert_eq!(window.get_git_catalog_rows().row_count(), 1);
        assert!(
            window
                .get_git_catalog_rows()
                .row_data(0)
                .unwrap()
                .switch_workspace
        );
        menu_commands::run(window, live, &palette_target, command, "");
        assert!(
            live.pending.borrow().is_none(),
            "the captured row must not retarget to the filtered row"
        );
        assert!(visible(window));
        run(Action::Open);
        assert!(
            matches!(*live.pending.borrow(), Some(Question::ConfirmRepositoryWorkspace(id)) if id == other)
        );
        assert!(window.get_question_text().contains("Archive"));
        answer_question(window, live, 1);
        assert_eq!(active_workspace(live), Some(source_workspace));
        assert!(visible(window));
        assert_eq!(window.get_git_catalog_filter(), "draft");

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
        assert!(window.get_tree_open());
        assert_eq!(window.get_left_tab(), LeftTab::Git.index());
        assert!(!window.get_git_repo_active());
        assert!(git_ui::chosen_folder().is_some_and(|path| git_ui::same_path(&path, &clone)));
        assert_eq!(
            fs::read_to_string(source.join("note.md")).unwrap(),
            "base\nunsaved\n"
        );

        run(Action::Enter);
        run(Action::Enter);
        wait_for_catalog();
        assert!(visible(window), "multiple repositories require a choice");
        assert_eq!(window.get_git_catalog_rows().row_count(), 3);

        switch_workspace(window, live, None);
        run(Action::Enter);
        wait_for_catalog();
        assert!(visible(window));
        assert!(
            window
                .get_git_catalog_rows()
                .iter()
                .all(|row| !row.switch_workspace)
        );
        window.set_git_catalog_selected(1);
        run(Action::Open);
        assert!(
            matches!(*live.pending.borrow(), Some(Question::ChooseRepositoryWorkspace(ref targets)) if targets.len() == 2)
        );
        answer_question(window, live, 0);
        assert!(active_workspace(live).is_some());
        assert!(!visible(window));
    }
}
