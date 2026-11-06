//! The merge resolver reads Git's unmerged index and uses the regular editor for the result.
use super::*;
use crate::git::{MergeConflict, MergeState, Resolution};
use crate::panel_actions::Action;

#[derive(Default)]
struct Resolver {
    merge: Option<(PathBuf, MergeState)>,
    selected: Option<MergeConflict>,
    version: usize,
    generation: u64,
    stamp: Option<file_io::FileStamp>,
}

thread_local! {
    static RESOLVER: RefCell<Resolver> = RefCell::default();
}

pub(crate) fn target() -> Option<(PathBuf, MergeState, Option<MergeConflict>)> {
    RESOLVER.with(|resolver| {
        let resolver = resolver.borrow();
        let (root, state) = resolver.merge.as_ref()?;
        Some((root.clone(), state.clone(), resolver.selected.clone()))
    })
}

pub(crate) fn publish(window: &AppWindow) {
    let next = git_ui::merge_snapshot();
    let changed = RESOLVER.with(|resolver| {
        let mut resolver = resolver.borrow_mut();
        if resolver.merge == next {
            return false;
        }
        let selected_path = resolver.selected.as_ref().map(|file| file.path.as_str());
        let same_merge = next.as_ref().zip(resolver.merge.as_ref()).is_some_and(
            |((root, state), (old_root, old))| {
                root == old_root && state.head == old.head && state.incoming == old.incoming
            },
        );
        let selected = next.as_ref().and_then(|(_, state)| {
            let at = same_merge
                .then(|| {
                    state
                        .conflicts
                        .iter()
                        .position(|file| Some(file.path.as_str()) == selected_path)
                })
                .flatten()
                .unwrap_or(0);
            state.conflicts.get(at).cloned()
        });
        if !same_merge || resolver.selected != selected {
            resolver.version = 3;
        }
        resolver.merge = next;
        resolver.selected = selected;
        true
    });
    if changed {
        let merge = target();
        window.set_git_merge_active(merge.is_some());
        if let Some((_, state, selected)) = merge {
            let selected = state
                .conflicts
                .iter()
                .position(|file| Some(file) == selected.as_ref());
            window.set_git_merge_selected(selected.map_or(-1, |at| at as i32));
            window.set_git_merge_ready(state.conflicts.is_empty());
            window.set_git_merge_head(state.head.into());
            window.set_git_merge_incoming(state.incoming.join(", ").into());
            window.set_git_merge_paths(ModelRc::new(VecModel::from(
                state
                    .conflicts
                    .iter()
                    .map(|file| SharedString::from(&file.path))
                    .collect::<Vec<_>>(),
            )));
        } else {
            window.set_git_merge_open(false);
            window.set_git_merge_ready(false);
            window.set_git_merge_selected(-1);
        }
    }
    if window.get_git_merge_open() {
        preview(window, changed);
    }
}

fn select(window: &AppWindow, at: i32) {
    let selected = RESOLVER.with(|resolver| {
        let mut resolver = resolver.borrow_mut();
        let (_, state) = resolver.merge.as_ref()?;
        let file = state.conflicts.get(usize::try_from(at).ok()?)?.clone();
        resolver.selected = Some(file);
        resolver.version = 3;
        Some(())
    });
    if selected.is_some() {
        window.set_git_merge_selected(at);
        preview(window, true);
    }
}

fn preview(window: &AppWindow, force: bool) {
    let Some((root, _, Some(file))) = target() else {
        return;
    };
    let path = root.join(&file.path);
    let stamp = file_io::FileStamp::read(&path).ok();
    window.set_git_merge_can_edit(path.is_file() && !path.is_symlink());
    window.set_git_merge_can_delete(file.can_delete());
    let read = RESOLVER.with(|resolver| {
        let mut resolver = resolver.borrow_mut();
        if !force && (resolver.version != 3 || resolver.stamp == stamp) {
            return None;
        }
        resolver.stamp = stamp;
        resolver.generation += 1;
        Some((resolver.generation, resolver.version))
    });
    let Some((generation, version)) = read else {
        return;
    };
    window.set_git_merge_version(version as i32);
    window.set_git_merge_body(pick("読み込んでいます…", "Reading…").into());
    let weak = window.as_weak();
    std::thread::spawn(move || {
        let missing = || {
            pick(
                "この版にファイルはありません。",
                "This version has no file.",
            )
            .to_owned()
        };
        let unreadable = || {
            pick(
                "テキストとしてプレビューできません。",
                "This version cannot be previewed as text.",
            )
            .to_owned()
        };
        let text = if let Some(version) = file.versions.get(version) {
            match version {
                None => missing(),
                Some(blob) => match git::conflict_blob(&root, blob) {
                    Ok(bytes) => file_io::decode(&bytes, MAX_DOCUMENT_CHARACTERS)
                        .map(|(text, _)| text)
                        .unwrap_or_else(|_| unreadable()),
                    Err(error) => error.to_string(),
                },
            }
        } else if path.is_symlink() {
            unreadable()
        } else if !path.exists() {
            missing()
        } else {
            file_io::read(&path, MAX_DOCUMENT_CHARACTERS)
                .map(|file| file.text)
                .unwrap_or_else(|_| unreadable())
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            if window.get_git_merge_open()
                && RESOLVER.with(|resolver| resolver.borrow().generation == generation)
            {
                window.set_git_merge_body(text.into());
            }
        });
    });
}

pub(crate) fn execute(window: &AppWindow, live: &Live, action: Action) {
    let Some((root, state, file)) = target() else {
        return;
    };
    if action == Action::GitMergeOpen {
        if !window.get_git_repo_active() {
            git_repo_ui::open(window, live);
        }
        if git_repo_ui::root().as_ref() == Some(&root) && window.get_git_repo_active() {
            window.set_git_merge_open(true);
            preview(window, true);
        }
        return;
    }
    let resolution = match action {
        Action::GitMergeEdit => {
            let Some(file) = file else { return };
            let path = root.join(&file.path);
            if git::merge_state(&root).ok().flatten().as_ref() != Some(&state)
                || !path.is_file()
                || path.is_symlink()
            {
                git_ui::notice(window, live, git::merge_changed().to_string());
                git_ui::refresh_soon(window, live);
                return;
            }
            git_repo_ui::close(window);
            window.set_tree_open(true);
            window.set_left_tab(LeftTab::Git.index());
            let focused = focused_pane(window);
            let pane = if focused.is_panel() {
                PaneId::from_index(focused.screen(window).panel_owner)
            } else {
                focused
            };
            open_path_in_pane(window, live, pane, &path, Opening::Kept);
            let document = live.states.document(pane);
            let needs_reload = {
                let file = document.file.borrow();
                file.path() == Some(path.as_path())
                    && !document.text.edited()
                    && file.external_change() == buffer::ExternalChange::Modified
            };
            if needs_reload {
                saving::reload_document(window, live, &document);
            }
            RESOLVER.with(|resolver| resolver.borrow_mut().version = 3);
            publish_left(window, live);
            return;
        }
        Action::GitMergeStage => Resolution::Stage,
        Action::GitMergeDelete => Resolution::Delete(
            file.as_ref()
                .and_then(|file| git::worktree_stamp(&root.join(&file.path))),
        ),
        Action::GitMergeContinue => Resolution::Continue,
        Action::GitMergeAbort => Resolution::Abort,
        _ => return,
    };
    let path = match resolution {
        Resolution::Stage | Resolution::Delete(_) => file.map(|file| file.path),
        Resolution::Continue | Resolution::Abort => None,
    };
    git_ui::act_on(
        window,
        live,
        root,
        git_ui::Action::ResolveMerge {
            state,
            path,
            resolution,
        },
    );
}

pub(crate) fn wire(window: &AppWindow) {
    let weak = window.as_weak();
    window.on_git_merge_file(move |at| {
        if let Some(window) = weak.upgrade() {
            select(&window, at);
        }
    });
    let weak = window.as_weak();
    window.on_git_merge_version_chosen(move |version| {
        if let Some(window) = weak.upgrade()
            && (0..4).contains(&version)
        {
            RESOLVER.with(|resolver| resolver.borrow_mut().version = version as usize);
            preview(&window, true);
        }
    });
    let weak = window.as_weak();
    window.on_git_merge_back(move || {
        if let Some(window) = weak.upgrade() {
            window.set_git_merge_open(false);
        }
    });
}
