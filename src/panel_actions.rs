//! Shared actions for the controls in the main window's panels.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    OutlineCollapse,
    OutlineExpand,
    TagsCollapse,
    TagsExpand,
    BookmarkAdd,
    BookmarkGroup,
    WorkspaceAttach,
    WorkspaceClone,
    WorkspaceReset,
    WorkspaceLeave,
    GitFetch,
    GitPull,
    GitPush,
    GitSync,
    GitCancel,
    GitCommit,
    DiffPrevious,
    DiffNext,
    DiffClose,
    DiffApply,
    DiffKeepLeft,
    DiffTakeRight,
    DiffCopyLeft,
    DiffCopyRight,
    BackupSelectAll,
    BackupDelete,
    PrintPrevious,
    PrintNext,
    PrintClose,
    PrintSubmit,
    SettingsExport,
    SettingsImport,
    SettingsReset,
}

impl Action {
    pub(crate) fn description(self) -> (&'static str, &'static str) {
        use Action::*;
        match self {
            OutlineCollapse | OutlineExpand => (
                "表示中の文書の見出しに適用します。",
                "Applies to the headings of the current document.",
            ),
            TagsCollapse | TagsExpand => ("タグの一覧に適用します。", "Applies to the tag list."),
            BookmarkAdd | BookmarkGroup => (
                "ブックマークの面で使えます。",
                "Available in the Bookmarks panel.",
            ),
            WorkspaceAttach | WorkspaceClone | WorkspaceReset | WorkspaceLeave => (
                "使用中のWorkspaceが対象です。",
                "Acts on the active workspace.",
            ),
            GitFetch | GitPull | GitPush | GitSync | GitCommit | GitCancel => (
                "選択したリポジトリが対象です。",
                "Acts on the selected repository.",
            ),
            DiffPrevious | DiffNext | DiffClose | DiffApply | DiffKeepLeft | DiffTakeRight
            | DiffCopyLeft | DiffCopyRight => (
                "比較中に割り当てたキーで使えます。",
                "Use an assigned shortcut while comparing.",
            ),
            BackupSelectAll | BackupDelete => (
                "バックアップ履歴で割り当てたキーを使えます。",
                "Use an assigned shortcut in Backup History.",
            ),
            PrintPrevious | PrintNext | PrintClose | PrintSubmit => (
                "印刷プレビューで割り当てたキーを使えます。",
                "Use an assigned shortcut in Print Preview.",
            ),
            SettingsExport | SettingsImport | SettingsReset => (
                "設定のGeneralで使えます。",
                "Available in General settings.",
            ),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Context {
    view: i32,
    workspace: Option<workspace::WorkspaceId>,
    repo: Option<PathBuf>,
    git_message: SharedString,
    git_amend: bool,
    branch: SharedString,
    comparison: (SharedString, SharedString),
    print_title: SharedString,
    comparison_rows: ModelRc<DiffRow>,
    difference: i32,
    backup_rows: ModelRc<BackupRow>,
}

impl Context {
    pub(crate) fn capture(window: &AppWindow, live: &Live) -> Self {
        Self {
            view: window.get_left_tab(),
            workspace: live
                .folder
                .borrow()
                .workspace
                .as_ref()
                .and_then(|runtime| runtime.borrow().active_workspace()),
            repo: git_ui::active_root(),
            git_message: window.get_git_message(),
            git_amend: window.get_git_amend(),
            branch: window.get_git_branch(),
            comparison: (window.get_diff_left_path(), window.get_diff_right_path()),
            print_title: window.get_print_title(),
            comparison_rows: window.get_diff_rows(),
            difference: window.get_diff_selected_row(),
            backup_rows: window.get_backup_history_rows(),
        }
    }

    pub(crate) fn valid(&self, window: &AppWindow, live: &Live, action: Action) -> bool {
        use Action::*;
        let current = Self::capture(window, live);
        match action {
            WorkspaceAttach | WorkspaceClone | WorkspaceReset | WorkspaceLeave => {
                self.view == current.view && self.workspace == current.workspace
            }
            GitCommit => {
                self.repo == current.repo
                    && self.branch == current.branch
                    && self.git_message == current.git_message
                    && self.git_amend == current.git_amend
            }
            GitFetch | GitPull | GitPush | GitSync | GitCancel => {
                self.repo == current.repo && self.branch == current.branch
            }
            DiffPrevious | DiffNext | DiffClose | DiffCopyLeft | DiffCopyRight => {
                self.comparison == current.comparison
            }
            DiffKeepLeft | DiffTakeRight | DiffApply => {
                self.comparison == current.comparison
                    && self.comparison_rows == current.comparison_rows
                    && self.difference == current.difference
            }
            BackupSelectAll | BackupDelete => {
                self.comparison == current.comparison && self.backup_rows == current.backup_rows
            }
            PrintPrevious | PrintNext | PrintClose | PrintSubmit => {
                self.print_title == current.print_title
            }
            _ => self.view == current.view,
        }
    }
}

pub(crate) fn enabled(window: &AppWindow, action: Action, document: bool, settings: bool) -> bool {
    use Action::*;
    if window.get_question_open() {
        return false;
    }
    let diff = window.get_diff_active();
    let print = window.get_print_active() && !diff;
    let repository = window.get_git_repo_active() && !diff && !print;
    let overlay = diff || print || repository;
    let shown =
        |tab: LeftTab| !overlay && window.get_tree_open() && window.get_left_tab() == tab.index();
    let git = (shown(LeftTab::Git) || repository) && window.get_git_mode() == 2;
    match action {
        OutlineCollapse | OutlineExpand => shown(LeftTab::Outline) && document,
        TagsCollapse | TagsExpand => shown(LeftTab::Tags) && window.get_tag_available(),
        BookmarkAdd | BookmarkGroup => shown(LeftTab::Bookmarks) && window.get_bookmark_available(),
        WorkspaceAttach | WorkspaceReset | WorkspaceLeave => {
            shown(LeftTab::Workspace) && window.get_workspace_active()
        }
        WorkspaceClone => {
            shown(LeftTab::Workspace)
                && window.get_workspace_active()
                && !window.get_workspace_cloning()
        }
        GitFetch => git && !window.get_git_busy() && window.get_git_has_commit(),
        GitPull => git && !window.get_git_busy() && window.get_git_has_upstream(),
        GitPush | GitSync => {
            git && !window.get_git_busy()
                && window.get_git_has_commit()
                && window.get_git_on_branch()
        }
        GitCancel => git && window.get_git_can_cancel(),
        GitCommit => {
            git && !window.get_git_busy()
                && !window.get_git_message().trim().is_empty()
                && (window.get_git_amend()
                    || window.get_git_staged_count() > 0
                    || window.get_git_change_count() > 0)
        }
        DiffPrevious => diff && window.get_diff_can_previous(),
        DiffNext => diff && window.get_diff_can_next(),
        DiffClose | DiffCopyLeft | DiffCopyRight => diff,
        DiffApply => diff && window.get_diff_merge_enabled() && window.get_diff_can_apply(),
        DiffKeepLeft | DiffTakeRight => {
            diff && window.get_diff_merge_enabled() && window.get_diff_selected_row() >= 0
        }
        BackupSelectAll => {
            diff && window.get_backup_history_active()
                && window.get_backup_history_rows().row_count() > 0
        }
        BackupDelete => {
            diff && window.get_backup_history_active() && window.get_backup_history_any()
        }
        PrintPrevious => print && !window.get_print_working() && window.get_print_at() > 0,
        PrintNext => {
            print
                && !window.get_print_working()
                && window.get_print_at() + print_step(window) < window.get_print_pages()
        }
        PrintClose => print,
        PrintSubmit => print && !window.get_print_working() && window.get_print_pages() > 0,
        SettingsExport | SettingsImport | SettingsReset => {
            !overlay
                && settings
                && window.get_settings_tab() == 0
                && window.get_settings_query().is_empty()
        }
    }
}

pub(crate) fn overlay_active(window: &AppWindow) -> bool {
    window.get_diff_active() || window.get_print_active() || window.get_git_repo_active()
}

fn print_step(window: &AppWindow) -> i32 {
    window.get_print_sheets().row_count().max(1) as i32
}

pub(crate) fn execute(window: &AppWindow, live: &Live, action: Action) {
    match action {
        Action::OutlineCollapse => fold_whole_outline(window, live, true),
        Action::OutlineExpand => fold_whole_outline(window, live, false),
        Action::TagsCollapse => fold_all_tags(window, live, true),
        Action::TagsExpand => fold_all_tags(window, live, false),
        Action::BookmarkAdd => window.invoke_bookmark_add_current(),
        Action::BookmarkGroup => window.invoke_bookmark_create_group(),
        Action::WorkspaceAttach => workspace_tree_command(window, live, 0),
        Action::WorkspaceClone => workspace_tree_command(window, live, 1),
        Action::WorkspaceReset => workspace_tree_command(window, live, 4),
        Action::WorkspaceLeave => workspace_leave_requested(window, live),
        Action::GitFetch => git_ui::fetch(window, live),
        Action::GitPull => git_ui::act(window, live, git_ui::Action::Pull),
        Action::GitPush => git_ui::push(window, live),
        Action::GitSync => git_ui::act(window, live, git_ui::Action::Sync),
        Action::GitCancel => git_ui::cancel(),
        Action::GitCommit => git_ui::commit(window, live),
        Action::DiffPrevious => window.invoke_diff_next_difference(false),
        Action::DiffNext => window.invoke_diff_next_difference(true),
        Action::DiffClose => window.invoke_diff_dismissed(),
        Action::DiffApply => window.invoke_diff_apply_merge(),
        Action::DiffKeepLeft => window.invoke_diff_choose_side(false),
        Action::DiffTakeRight => window.invoke_diff_choose_side(true),
        Action::DiffCopyLeft => window.invoke_diff_copy_side(false),
        Action::DiffCopyRight => window.invoke_diff_copy_side(true),
        Action::BackupSelectAll => window.invoke_backup_history_all_toggled(),
        Action::BackupDelete => window.invoke_backup_history_delete(),
        Action::PrintPrevious => {
            window.invoke_print_turned(window.get_print_at() - print_step(window))
        }
        Action::PrintNext => window.invoke_print_turned(window.get_print_at() + print_step(window)),
        Action::PrintClose => window.invoke_print_dismissed(),
        Action::PrintSubmit => window.invoke_print_requested(),
        Action::SettingsExport => window.invoke_settings_export_requested(),
        Action::SettingsImport => window.invoke_settings_import_requested(),
        Action::SettingsReset => window.invoke_reset_all_requested(),
    }
}

pub(crate) fn wire(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    let live = live.clone();
    window.on_surface_action(move |id| {
        let Some(window) = weak.upgrade() else { return };
        let Some(action) =
            actions::index_of(&id).and_then(|at| actions::all()[at].command.as_ref())
        else {
            return;
        };
        if let menu_commands::Command::PanelAction(action) = action {
            queue(&window, &live, *action);
        }
    });
}

pub(crate) fn queue(window: &AppWindow, live: &Live, action: Action) {
    let target = menu_commands::Target::for_surface(window, live);
    let weak = window.as_weak();
    let live = live.clone();
    Timer::single_shot(Duration::ZERO, move || {
        if let Some(window) = weak.upgrade() {
            let command = menu_commands::Command::PanelAction(action);
            let title = actions::of_command(&command)
                .map(|def| pick(&def.title.0, &def.title.1))
                .unwrap_or_default();
            menu_commands::run(&window, &live, &target, command, title);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu_commands::{Command, Scene, Target, run};
    use crate::saving::Harness;

    #[test]
    fn comparison_buttons_and_keys_share_availability_without_opening_a_menu() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let window = &h.window;
        let live = &h.live;
        let hit = Rc::new(Cell::new(0));
        let received = hit.clone();
        window.on_diff_next_difference(move |forward| {
            if forward {
                received.set(received.get() + 1);
            }
        });
        wire(window, live);
        shortcuts::wire(window, live);
        let command = Command::PanelAction(Action::DiffNext);
        let target = Target::for_surface(window, live);
        assert!(!Scene::read(window, live, &target).enabled(&command));
        window.set_diff_active(true);
        window.set_diff_can_next(true);
        window.set_shortcut_bindings("comparison.next=Ctrl+Alt+9;v=5".into());

        window.invoke_surface_action("comparison.next".into());
        slint::platform::update_timers_and_animations();
        assert!(window.invoke_panel_shortcut("9".into(), true, true, false));
        slint::platform::update_timers_and_animations();
        assert_eq!(hit.get(), 2);
        assert_eq!(window.get_menu_generation(), 0);
    }

    #[test]
    fn apply_requires_the_original_comparison_selection_and_rows() {
        let (h, _) = Harness::new(|weak| OpenDocument::untitled(1, weak));
        let window = &h.window;
        let live = &h.live;
        window.set_diff_active(true);
        window.set_diff_merge_enabled(true);
        window.set_diff_can_apply(true);
        window.set_diff_selected_row(0);
        window.set_diff_rows(ModelRc::new(VecModel::from(vec![DiffRow::default(), DiffRow::default()])));
        let received = Rc::new(Cell::new(0));
        let hit = received.clone();
        window.on_diff_apply_merge(move || hit.set(hit.get() + 1));
        let action = Command::PanelAction(Action::DiffApply);

        let target = Target::for_surface(window, live);
        run(window, live, &target, action.clone(), "");
        assert_eq!(received.get(), 1, "unchanged target");

        window.set_diff_selected_row(1);
        run(window, live, &target, action.clone(), "");
        assert_eq!(received.get(), 1, "different selected row");

        window.set_diff_selected_row(0);
        window.set_diff_rows(ModelRc::new(VecModel::from(vec![DiffRow::default(), DiffRow::default()])));
        run(window, live, &target, action.clone(), "");
        assert_eq!(received.get(), 1, "new comparison rows at the same index");

        let current = Target::for_surface(window, live);
        window.set_diff_active(false);
        run(window, live, &current, action, "");
        assert_eq!(received.get(), 1, "comparison closed after capturing the target");
    }
}
