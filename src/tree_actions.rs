//! Actions for the file tree shared by Explorer and Workspace.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Entry(TreeCommand),
    RevealDocument,
    CollapseAll,
    Refresh,
    Search,
    Previous,
    Next,
    Activate,
    Left,
    Right,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Target {
    view: LeftTab,
    workspace: Option<workspace::WorkspaceId>,
    roots: Vec<PathBuf>,
    selected: Option<PathBuf>,
}

impl Target {
    pub(crate) fn capture(window: &AppWindow, live: &Live) -> Self {
        let folder = live.folder.borrow();
        Self {
            view: LeftTab::from_index(window.get_left_tab()),
            workspace: folder
                .workspace
                .as_ref()
                .and_then(|runtime| runtime.borrow().active_workspace()),
            roots: tree_roots(&folder).0,
            selected: folder.selected.clone(),
        }
    }

    pub(crate) fn valid(&self, window: &AppWindow, live: &Live) -> bool {
        *self == Self::capture(window, live)
    }

    fn available(&self, window: &AppWindow, live: &Live) -> bool {
        let shown = match self.view {
            LeftTab::Explorer => !window.get_workspace_active(),
            LeftTab::Workspace => window.get_workspace_active(),
            _ => false,
        };
        shown
            && window.get_tree_open()
            && !self.roots.is_empty()
            && !window.get_question_open()
            && !window.get_print_active()
            && !window.get_diff_active()
            && live.folder.borrow().creating.is_none()
    }

    fn selected_row(&self, live: &Live) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        live.tree_paths
            .borrow()
            .iter()
            .position(|path| path == selected)
    }

    pub(crate) fn enabled(&self, window: &AppWindow, live: &Live, action: Action) -> bool {
        if !self.available(window, live) {
            return false;
        }
        let selected = || self.selected.as_deref().filter(|path| path.exists());
        match action {
            Action::Entry(TreeCommand::NewFile | TreeCommand::NewFolder) => {
                selected().is_some() || (self.selected.is_none() && self.roots.len() == 1)
            }
            Action::Entry(TreeCommand::Rename | TreeCommand::Duplicate | TreeCommand::Delete) => {
                selected().is_some_and(|path| !is_registered_root(live, path))
            }
            Action::Entry(_) => selected().is_some(),
            Action::RevealDocument | Action::CollapseAll | Action::Refresh | Action::Search => true,
            Action::Previous | Action::Next => {
                !window.get_tree_searching() && !live.tree_paths.borrow().is_empty()
            }
            Action::Activate | Action::Left | Action::Right => {
                !window.get_tree_searching()
                    && selected().is_some()
                    && self.selected_row(live).is_some()
            }
        }
    }
}

pub(crate) fn execute(
    window: &AppWindow,
    live: &Live,
    target: &Target,
    pane: PaneId,
    action: Action,
) {
    match action {
        Action::Entry(command) => tree_command(window, live, command),
        Action::RevealDocument => explorer_command(window, live, pane, 0),
        Action::CollapseAll => explorer_command(window, live, pane, 1),
        Action::Refresh => explorer_command(window, live, pane, 2),
        Action::Search => toggle_tree_search(window, live),
        Action::Previous | Action::Next => {
            let count = live.tree_paths.borrow().len();
            let at = target.selected_row(live);
            let next = if action == Action::Previous {
                at.map_or(count.saturating_sub(1), |at| at.saturating_sub(1))
            } else {
                at.map_or(0, |at| (at + 1).min(count.saturating_sub(1)))
            };
            select(window, live, next);
        }
        Action::Activate | Action::Left | Action::Right => {
            let Some(at) = target.selected_row(live) else {
                return;
            };
            let rows = window.get_left_rows();
            let Some(row) = rows.row_data(at) else { return };
            match action {
                Action::Activate if row.is_root => {}
                Action::Activate if row.folder => set_expanded(window, live, target, !row.open),
                Action::Activate => {
                    let had_focus = window.get_tree_focused();
                    activate_tree_row(window, live, at);
                    if had_focus {
                        restore_focus(window);
                    } else {
                        restore_editor_focus(window);
                    }
                }
                Action::Left if row.folder && row.open && !row.is_root => {
                    set_expanded(window, live, target, false)
                }
                Action::Left if row.parent >= 0 => select(window, live, row.parent as usize),
                Action::Right if row.folder && !row.open => {
                    set_expanded(window, live, target, true)
                }
                Action::Right
                    if rows
                        .row_data(at + 1)
                        .is_some_and(|child| child.parent == at as i32) =>
                {
                    select(window, live, at + 1);
                }
                _ => {}
            }
        }
    }
}

pub(crate) fn restore_focus(window: &AppWindow) {
    window.set_tree_focus_generation(window.get_tree_focus_generation().wrapping_add(1));
}

fn select(window: &AppWindow, live: &Live, at: usize) {
    if at < live.tree_paths.borrow().len() {
        pick_tree_row(live, at);
        window.set_tree_selected(at as i32);
        window.set_tree_key_reveal(window.get_tree_key_reveal().wrapping_add(1));
    }
}

fn set_expanded(window: &AppWindow, live: &Live, target: &Target, expanded: bool) {
    let Some(path) = &target.selected else { return };
    {
        let mut folder = live.folder.borrow_mut();
        if expanded {
            folder.expanded.insert(path.clone());
        } else {
            folder.expanded.remove(path);
        }
    }
    publish_tree(window, live);
    write_session(window, live);
}

/// Let Slint finish handling the click or key before changing the row model.
pub(crate) fn queue(
    window: &AppWindow,
    live: &Live,
    target: menu_commands::Target,
    action: Action,
) {
    let weak = window.as_weak();
    let live = live.clone();
    slint::Timer::single_shot(Duration::ZERO, move || {
        if let Some(window) = weak.upgrade() {
            let command = menu_commands::Command::Tree(action);
            let name = actions::of_command(&command)
                .map(|def| pick(&def.title.0, &def.title.1))
                .unwrap_or_default();
            menu_commands::run(&window, &live, &target, command, name);
        }
    });
}

pub(crate) fn publish_context(window: &AppWindow, live: &Live, target: &menu_commands::Target) {
    let scene = menu_commands::Scene::read(window, live, target);
    let items = (0..8)
        .filter_map(TreeCommand::from_index)
        .map(|entry| {
            let command = menu_commands::Command::Tree(Action::Entry(entry));
            let title = actions::of_command(&command)
                .map(|def| pick(&def.title.0, &def.title.1))
                .unwrap_or_default();
            TreeMenuItem {
                title: title.into(),
                enabled: scene.enabled(&command),
                shortcut: shortcuts::label_of(window, &command).into(),
            }
        })
        .collect::<Vec<_>>();
    window.set_tree_context_items(ModelRc::new(VecModel::from(items)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saving::{Harness, attach_workspace, open_under, scratch_directory};
    use menu_commands::{Command, Scene, Target as CommandTarget, run};

    #[test]
    fn shared_actions_reject_a_changed_target_and_navigate_visible_rows() {
        let root = scratch_directory("tree-actions");
        let folder = root.join("notes");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("first.md"), "").unwrap();
        let (harness, _) = Harness::new(|window| open_under(&root, "top.md", "", window));
        let window = &harness.window;
        let live = &harness.live;
        live.folder.borrow_mut().root = Some(root.clone());
        window.set_tree_open(true);
        publish_left(window, live);

        let perform = |action| {
            run(
                window,
                live,
                &CommandTarget::for_tree(window, live),
                Command::Tree(action),
                "",
            )
        };
        perform(Action::Next);
        assert_eq!(live.folder.borrow().selected.as_ref(), Some(&folder));
        let old = CommandTarget::for_tree(window, live);
        perform(Action::Right);
        assert!(live.folder.borrow().expanded.contains(&folder));
        perform(Action::Right);
        assert_eq!(
            live.folder.borrow().selected.as_ref(),
            Some(&folder.join("first.md"))
        );
        run(
            window,
            live,
            &old,
            Command::Tree(Action::Entry(TreeCommand::Rename)),
            "",
        );
        assert!(!window.get_question_open());
        assert!(live.folder.borrow().creating.is_none());
        perform(Action::Left);
        perform(Action::Left);
        assert_eq!(live.folder.borrow().selected.as_ref(), Some(&folder));
        assert!(!live.folder.borrow().expanded.contains(&folder));
        perform(Action::Next);
        perform(Action::Entry(TreeCommand::Rename));
        assert!(live.folder.borrow().creating.is_some() || window.get_question_open());
    }

    #[test]
    fn workspace_root_navigation_preserves_its_protections() {
        let root = scratch_directory("tree-actions-root");
        let (harness, _) = Harness::new(|window| open_under(&root, "top.md", "", window));
        let window = &harness.window;
        let live = &harness.live;
        attach_workspace(live, &root, workspace::SaveMode::Recovery);
        window.set_workspace_active(true);
        window.set_tree_open(true);
        window.set_left_tab(LeftTab::Workspace.index());
        publish_left(window, live);
        pick_tree_row(live, 0);
        let target = CommandTarget::for_tree(window, live);
        let scene = Scene::read(window, live, &target);
        for command in [
            TreeCommand::Rename,
            TreeCommand::Duplicate,
            TreeCommand::Delete,
        ] {
            assert!(!scene.enabled(&Command::Tree(Action::Entry(command))));
        }
        assert!(scene.enabled(&Command::Tree(Action::Entry(TreeCommand::NewFile))));
        run(window, live, &target, Command::Tree(Action::Right), "");
        assert_eq!(window.get_left_rows().row_count(), 2);
        assert_eq!(
            live.folder.borrow().selected,
            live.tree_paths.borrow().get(1).cloned()
        );
    }
}
