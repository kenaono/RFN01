//! Worktree selection shares the Git repository and Workspace action paths.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Show,
    Back,
    Open,
    Create,
}

#[derive(Default)]
struct State {
    root: Option<PathBuf>,
    head: Option<String>,
    rows: Vec<git::Worktree>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

#[derive(Clone)]
pub(crate) struct Target {
    root: PathBuf,
    head: Option<String>,
    selected: Option<git::Worktree>,
    open: bool,
    parent: SharedString,
    folder: SharedString,
    branch: SharedString,
}

impl Target {
    pub(crate) fn capture(window: &AppWindow) -> Option<Self> {
        if !window.get_git_repo_active() {
            return None;
        }
        STATE.with(|state| {
            let state = state.borrow();
            let selected = usize::try_from(window.get_git_worktree_selected()).ok()
                .and_then(|index| state.rows.get(index)).cloned();
            Some(Self {
                root: state.root.clone()?,
                head: state.head.clone(),
                selected,
                open: window.get_git_worktrees_open(),
                parent: window.get_git_worktree_parent(),
                folder: window.get_git_worktree_folder(),
                branch: window.get_git_worktree_branch(),
            })
        })
    }

    pub(crate) fn valid(&self, current: &Self, action: Action) -> bool {
        self.root == current.root && self.open == current.open && match action {
            Action::Open => self.selected == current.selected,
            Action::Create => self.head == current.head && self.parent == current.parent
                && self.folder == current.folder && self.branch == current.branch,
            Action::Show | Action::Back => true,
        }
    }

    pub(crate) fn enabled(&self, window: &AppWindow, action: Action) -> bool {
        let idle = !window.get_git_busy();
        match action {
            Action::Show => !self.open,
            Action::Back => self.open,
            Action::Open => self.open && idle && window.get_workspace_active()
                && self.selected.as_ref().is_some_and(|tree| {
                    !tree.bare && !tree.prunable && !git_ui::same_path(&tree.path, &self.root)
                }),
            Action::Create => self.open && idle && self.head.is_some()
                && !self.parent.is_empty() && !self.folder.trim().is_empty()
                && !self.branch.trim().is_empty(),
        }
    }

    pub(crate) fn execute(&self, window: &AppWindow, live: &Live, action: Action) {
        match action {
            Action::Show => {
                window.set_git_worktrees_open(true);
                git_repo_ui::refresh(window, live);
            }
            Action::Back => window.set_git_worktrees_open(false),
            Action::Open => {
                if let Some((workspace, selected)) = active_workspace(live).zip(self.selected.as_ref()) {
                    request_open(window, live, OpenWorktree {
                        source: self.root.clone(),
                        target: selected.path.clone(),
                        workspace,
                    });
                }
            }
            Action::Create => {
                if let Some(head) = &self.head {
                    git_ui::create_worktree(
                        window, live, self.root.clone(), PathBuf::from(self.parent.as_str()),
                        self.folder.trim().to_owned(), self.branch.trim().to_owned(), head.clone(),
                    );
                }
            }
        }
    }
}

pub(crate) fn publish(
    window: &AppWindow,
    root: Option<&Path>,
    head: Option<&str>,
    read: Option<&Result<Vec<git::Worktree>, git::GitError>>,
) {
    let rows = read.and_then(|result| result.as_ref().ok()).map(Vec::as_slice).unwrap_or_default();
    let (changed_root, selected) = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let changed_root = state.root.as_deref() != root;
        let previous = usize::try_from(window.get_git_worktree_selected()).ok()
            .and_then(|index| state.rows.get(index));
        let selected = previous.filter(|_| !changed_root)
            .and_then(|previous| rows.iter().position(|tree| git_ui::same_path(&tree.path, &previous.path)))
            .or_else(|| rows.iter().position(|tree| root.is_some_and(|root| git_ui::same_path(&tree.path, root))));
        state.root = root.map(Path::to_path_buf);
        state.head = head.map(str::to_owned);
        state.rows = rows.to_vec();
        (changed_root, selected)
    });
    if changed_root {
        window.set_git_worktree_parent("".into());
        window.set_git_worktree_folder("".into());
        window.set_git_worktree_branch("".into());
    }
    window.set_git_worktree_rows(ModelRc::new(VecModel::from(rows.iter().map(|tree| {
        let current = root.is_some_and(|root| git_ui::same_path(&tree.path, root));
        let label = if tree.bare {
            pick("Bareリポジトリ", "Bare repository").to_owned()
        } else if let Some(branch) = &tree.branch {
            branch.clone()
        } else if let Some(head) = &tree.head {
            let short = head.chars().take(8).collect::<String>();
            say!("分離HEAD · {short}", "Detached HEAD · {short}")
        } else {
            pick("ブランチなし", "No branch").to_owned()
        };
        GitWorktreeRow {
            path: tree.path.to_string_lossy().as_ref().into(),
            branch: label.into(),
            current,
            status: if tree.prunable {
                pick("作業先を確認できません", "Worktree unavailable").into()
            } else if tree.locked {
                pick("ロック済み", "Locked").into()
            } else {
                "".into()
            },
        }
    }).collect::<Vec<_>>())));
    window.set_git_worktree_selected(selected.map_or(-1, |index| index as i32));
    window.set_git_worktree_has_head(head.is_some());
    window.set_git_worktree_note(match read {
        None => pick("Worktreeを読み込んでいます…", "Reading worktrees…").into(),
        Some(Err(error)) => error.to_string().into(),
        Some(Ok(_)) => "".into(),
    });
}

fn active_workspace(live: &Live) -> Option<workspace::WorkspaceId> {
    live.folder.borrow().workspace.as_ref()?.borrow().active_workspace()
}

#[derive(Clone, Debug)]
pub(crate) struct OpenWorktree {
    source: PathBuf,
    target: PathBuf,
    workspace: workspace::WorkspaceId,
}

fn request_open(window: &AppWindow, live: &Live, request: OpenWorktree) {
    if git_ui::workspace_folders(live).iter().any(|root| git_ui::same_path(root, &request.target)) {
        open_worktree(window, live, request);
        return;
    }
    let path = request.target.display();
    ask_question(
        window, live, Question::GitOpenWorktree(request.clone()),
        say!("{path}\n\nこのWorktreeを現在のWorkspaceに追加して開きますか？",
             "{path}\n\nAdd this worktree to the current Workspace and open it?"),
        &[pick("追加して開く", "Add and Open"), cancel()], -1,
    );
}

pub(crate) fn open_worktree(window: &AppWindow, live: &Live, request: OpenWorktree) {
    if !window.get_git_worktrees_open() || window.get_git_busy()
        || git_repo_ui::root().as_ref() != Some(&request.source)
        || active_workspace(live) != Some(request.workspace)
    {
        git_ui::notice(window, live, git::worktree_changed().to_string());
        return;
    }
    let path = match git::check_worktree(&request.source, &request.target) {
        Ok(path) => path,
        Err(error) => {
            git_ui::notice(window, live, error.to_string());
            git_repo_ui::refresh(window, live);
            return;
        }
    };
    if !git_ui::workspace_folders(live).iter().any(|root| git_ui::same_path(root, &path)) {
        let Some(runtime) = live.folder.borrow().workspace.clone() else { return };
        let added = runtime.borrow_mut().edit(|registry| registry.add_root(request.workspace, &path));
        if let Err(error) = added {
            git_ui::notice(window, live, workspace_edit_error_message(&error));
            return;
        }
        refresh_workspace_tree(window, live, request.workspace);
        publish_workspace_manager(window, live);
    }
    git_ui::select_registered_folder(window, live, &path);
}

pub(crate) fn wire(window: &AppWindow) {
    let weak = window.as_weak();
    window.on_git_worktree_browse(move || {
        let weak = weak.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() && window.get_git_worktrees_open()
                && let Some(path) = file_dialog::worktree_parent(
                    ime::window_handle(&window),
                    git_repo_ui::root().as_deref().and_then(Path::parent),
                )
            {
                window.set_git_worktree_parent(path.to_string_lossy().as_ref().into());
                git_repo_ui::regain_focus(&window);
            }
        });
    });
}
