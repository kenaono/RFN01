//! 書き手の求め 2026-09-22: the Bookmark View, and adding a bookmark from the
//! outline, the body's menu and the view's own button — connected to the
//! window. The tree and its rules are [`bookmarks`]; where a Workspace's tree
//! is kept is [`workspace_ui::Runtime`].
use super::*;
use crate::bookmarks::{Bookmark, NameError, Place, Tree};

fn runtime(live: &Live) -> Option<Rc<RefCell<workspace_ui::Runtime>>> {
    live.folder.borrow().workspace.clone()
}

/// Read the active Workspace's tree. `None` when no Workspace is active — or
/// when the runtime is busy, which only a caller already holding it could
/// make it, and that caller draws again when it is done.
fn read<T>(live: &Live, look: impl FnOnce(&Tree, &[PathBuf]) -> T) -> Option<T> {
    let runtime = runtime(live)?;
    let mut runtime = runtime.try_borrow_mut().ok()?;
    let roots = runtime.active_roots();
    let tree = runtime.bookmarks()?;
    Some(look(tree, &roots))
}

/// Change the active Workspace's tree, write it to its file and draw the view
/// again.
fn change<T>(window: &AppWindow, live: &Live, edit: impl FnOnce(&mut Tree) -> T) -> Option<T> {
    let runtime = runtime(live)?;
    let value = {
        let mut runtime = runtime.borrow_mut();
        let value = edit(runtime.bookmarks()?);
        if let Err(error) = runtime.save_bookmarks() {
            window.tell(
                say!(
                    "ブックマークを保存できませんでした: {error}",
                    "Could not save the bookmarks: {error}"
                )
                .into(),
            );
        }
        value
    };
    publish(window, live);
    Some(value)
}

/// The bookmark lit now, if one is. **Asked without insisting**: the view is
/// drawn again when the active Workspace changes, and that can happen while a
/// caller holds the cache — the row simply is not chosen until the next draw.
fn lit(live: &Live) -> Option<Bookmark> {
    let cache = live.cache.try_borrow().ok()?;
    cache
        .bookmark_mark
        .as_ref()
        .map(|mark| mark.bookmark.clone())
}

/// Draw the view: its rows, the group names "Move to" offers, the order and
/// which row is the bookmark lit now.
pub(crate) fn publish(window: &AppWindow, live: &Live) {
    let filter = window.get_bookmark_filter().to_string();
    let lit = lit(live);
    let drawn = read(live, |tree, roots| {
        let rows = tree.rows(&filter);
        let selected = lit
            .as_ref()
            .and_then(|bookmark| tree.find(bookmark))
            .and_then(|place| rows.iter().position(|row| row.place == place));
        let drawn = rows
            .iter()
            .map(|row| BookmarkRow {
                title: row.title.clone().into(),
                group: row.group,
                open: row.open,
                depth: row.depth,
                tip: tree
                    .bookmark(row.place)
                    .map(|item| bookmarks::link_label(&item.path, &item.heading, roots))
                    .unwrap_or_default()
                    .into(),
            })
            .collect::<Vec<_>>();
        let groups = tree
            .group_choices()
            .into_iter()
            .map(|(_, name)| SharedString::from(name))
            .collect::<Vec<_>>();
        (drawn, groups, tree.descending, selected)
    });
    let Some((drawn, groups, descending, selected)) = drawn else {
        window.set_bookmark_available(false);
        window.set_bookmark_rows(ModelRc::new(VecModel::from(Vec::<BookmarkRow>::new())));
        window.set_bookmark_groups(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        window.set_bookmark_selected(-1);
        return;
    };
    window.set_bookmark_available(true);
    window.set_bookmark_rows(ModelRc::new(VecModel::from(drawn)));
    window.set_bookmark_groups(ModelRc::new(VecModel::from(groups)));
    window.set_bookmark_descending(descending);
    window.set_bookmark_selected(selected.map_or(-1, |at| at as i32));
}

/// What a row of the view stands for.
fn place_of_row(window: &AppWindow, live: &Live, row: i32) -> Option<Place> {
    let filter = window.get_bookmark_filter().to_string();
    let row = usize::try_from(row).ok()?;
    read(live, |tree, _| {
        tree.rows(&filter).get(row).map(|row| row.place)
    })
    .flatten()
}

/// Where the group "Move to" offers at `choice` is stored; -1 is the root.
fn group_of_choice(live: &Live, choice: i32) -> Option<usize> {
    let choice = usize::try_from(choice).ok()?;
    read(live, |tree, _| {
        tree.group_choices().get(choice).map(|(at, _)| *at)
    })
    .flatten()
}

// --- Adding ---------------------------------------------------------------

/// Ask for a new bookmark on `document`: the heading at `heading` of its
/// outline, or the whole file.
fn offer(window: &AppWindow, live: &Live, document: &OpenDocument, heading: Option<usize>) {
    let Some(path) = document.file.borrow().path().map(Path::to_path_buf) else {
        window.tell(
            say!(
                "ブックマークを付けるには、先にファイルを保存してください。",
                "Save the file first to bookmark it."
            )
            .into(),
        );
        return;
    };
    let headings = document::outline(&document.text.borrow());
    let chain = heading
        .map(|index| bookmarks::heading_path(&headings, index))
        .unwrap_or_default();
    let Some((link, groups)) = read(live, |tree, roots| {
        let link = bookmarks::link_label(&path, &chain, roots);
        (link, tree.group_choices())
    }) else {
        window.tell(
            say!(
                "ブックマークはWorkspaceに保存されます。Workspaceを開いてください。",
                "Bookmarks belong to a workspace. Open a workspace to use them."
            )
            .into(),
        );
        return;
    };
    let title = default_title(&path, &chain);
    let mut names = vec![SharedString::from(root_label())];
    names.extend(
        groups
            .iter()
            .map(|(_, name)| SharedString::from(name.as_str())),
    );
    window.set_question_bookmark_link(link.into());
    window.set_question_bookmark_groups(ModelRc::new(VecModel::from(names)));
    window.set_question_bookmark_group(0);
    let groups = groups.into_iter().map(|(at, _)| at).collect();
    ask_for_name(
        window,
        live,
        Question::AddBookmark {
            path,
            heading: chain,
            groups,
        },
        pick("ブックマークを追加", "Add Bookmark").to_owned(),
        &title,
    );
}

fn root_label() -> &'static str {
    pick("（Root）", "(Root)")
}

/// What a bookmark is called until the writer says otherwise: the heading,
/// or the file's name.
fn default_title(path: &Path, chain: &[String]) -> String {
    chain.last().cloned().unwrap_or_else(|| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}

/// The body's right-click "Add Bookmark…": the heading the caret is under, or
/// the file.
pub(crate) fn offer_from_pane(window: &AppWindow, live: &Live, pane: i32) {
    let id = PaneId::from_index(pane);
    let document = live.states.document(id);
    let state = live.states.of(id);
    let heading = {
        let source = document.text.borrow();
        let caret = id.caret_byte(&state, &source);
        bookmarks::heading_at(&document::outline(&source), caret)
    };
    offer(window, live, &document, heading);
}

/// The view's "Bookmark Active Tab": the same, for the pane the writer is in.
pub(crate) fn offer_active(window: &AppWindow, live: &Live) {
    offer_from_pane(window, live, focused_pane(window).index());
}

/// The outline's right-click "Add Bookmark…", for the heading on that row.
pub(crate) fn offer_from_outline(window: &AppWindow, live: &Live, row: i32) {
    let document = live.active(window);
    let headings = document::outline(&document.text.borrow());
    let Ok(row) = usize::try_from(row) else {
        return;
    };
    let Some(heading) = outline_heading_of_row(live, &document, &headings, row) else {
        return;
    };
    offer(window, live, &document, Some(heading));
}

/// The dialog's OK.
pub(crate) fn add(
    window: &AppWindow,
    live: &Live,
    path: PathBuf,
    heading: Vec<String>,
    groups: Vec<usize>,
) {
    let typed = window.get_question_name().trim().to_owned();
    let title = if typed.is_empty() {
        default_title(&path, &heading)
    } else {
        typed
    };
    let choice = window.get_question_bookmark_group();
    let group = usize::try_from(choice - 1)
        .ok()
        .and_then(|at| groups.get(at).copied());
    let bookmark = Bookmark {
        title,
        path,
        heading,
    };
    change(window, live, |tree| tree.add(bookmark, group));
}

// --- The view's own commands ------------------------------------------------

fn name_refused(window: &AppWindow, error: NameError) {
    window.tell(
        match error {
            NameError::Empty => pick("名前が空です。", "The name is empty."),
            NameError::Taken => pick(
                "同じ名前のグループがあります。",
                "A group with that name already exists.",
            ),
        }
        .into(),
    );
}

pub(crate) fn ask_new_group(window: &AppWindow, live: &Live) {
    if read(live, |_, _| ()).is_none() {
        return;
    }
    ask_for_name(
        window,
        live,
        Question::NewBookmarkGroup,
        pick("新しいグループの名前", "Name of the new bookmark group").to_owned(),
        "",
    );
}

pub(crate) fn create_group(window: &AppWindow, live: &Live) {
    let name = window.get_question_name().to_string();
    if let Some(Err(error)) = change(window, live, |tree| tree.add_group(&name)) {
        name_refused(window, error);
    }
}

pub(crate) fn ask_rename(window: &AppWindow, live: &Live, row: i32) {
    let Some(place) = place_of_row(window, live, row) else {
        return;
    };
    let current = read(live, |tree, _| match place {
        Place::Group(group) => tree.groups.get(group).map(|held| held.name.clone()),
        Place::Item { .. } => tree.bookmark(place).map(|item| item.title.clone()),
    })
    .flatten()
    .unwrap_or_default();
    let asked = match place {
        Place::Group(_) => pick("グループの新しい名前", "New name for the group"),
        Place::Item { .. } => pick("ブックマークの新しいタイトル", "New title for the bookmark"),
    };
    ask_for_name(
        window,
        live,
        Question::RenameBookmark(place),
        asked.to_owned(),
        &current,
    );
}

pub(crate) fn rename(window: &AppWindow, live: &Live, place: Place) {
    let name = window.get_question_name().to_string();
    // The lit bookmark is found again by what it is, so it follows the rename.
    let before = read(live, |tree, _| tree.bookmark(place).cloned()).flatten();
    let renamed = change(window, live, |tree| {
        tree.rename(place, &name)?;
        Ok(tree.bookmark(place).cloned())
    });
    match renamed {
        Some(Err(error)) => name_refused(window, error),
        Some(Ok(Some(after))) => {
            let mut cache = live.cache.borrow_mut();
            if let Some(mark) = cache.bookmark_mark.as_mut() {
                if Some(&mark.bookmark) == before.as_ref() {
                    mark.bookmark = after;
                }
            }
            drop(cache);
            publish(window, live);
        }
        _ => {}
    }
}

/// Delete a row: a bookmark at once, a group only once the writer has said so
/// — it takes its bookmarks with it.
pub(crate) fn ask_remove(window: &AppWindow, live: &Live, row: i32) {
    let Some(place) = place_of_row(window, live, row) else {
        return;
    };
    match place {
        Place::Group(group) => {
            let Some((name, count)) = read(live, |tree, _| {
                let held = tree.groups.get(group)?;
                Some((held.name.clone(), held.items.len()))
            })
            .flatten() else {
                return;
            };
            ask_question(
                window,
                live,
                Question::DeleteBookmarkGroup(group),
                say!(
                    "グループ「{name}」を削除しますか？\n\n中のブックマーク{count}件も削除します。",
                    "Delete the group \"{name}\"?\n\nIts {count} bookmark(s) are deleted with it."
                ),
                &[pick("削除", "Delete"), cancel()],
                0,
            );
        }
        Place::Item { .. } => remove(window, live, place),
    }
}

pub(crate) fn remove(window: &AppWindow, live: &Live, place: Place) {
    change(window, live, |tree| tree.remove(place));
    // A lit bookmark that is no longer in the tree has nothing to light.
    let gone = lit(live).is_some_and(|bookmark| {
        read(live, |tree, _| tree.find(&bookmark))
            .flatten()
            .is_none()
    });
    if gone {
        let_go(window, &live.cache, &live.states);
        publish(window, live);
    }
}

pub(crate) fn toggle(window: &AppWindow, live: &Live, row: i32) {
    if let Some(Place::Group(group)) = place_of_row(window, live, row) {
        change(window, live, |tree| tree.toggle(group));
    }
}

pub(crate) fn toggle_order(window: &AppWindow, live: &Live) {
    change(window, live, |tree| tree.descending = !tree.descending);
}

pub(crate) fn move_to(window: &AppWindow, live: &Live, row: i32, choice: i32) {
    let Some(place) = place_of_row(window, live, row) else {
        return;
    };
    let to = group_of_choice(live, choice);
    change(window, live, |tree| tree.move_to(place, to));
}

/// A bookmark carried and let go on a row: into the group that row is, or is
/// in — the root when it is a root bookmark or no row at all.
pub(crate) fn dropped(window: &AppWindow, live: &Live, row: i32, onto: i32) {
    let Some(place) = place_of_row(window, live, row) else {
        return;
    };
    let to = match place_of_row(window, live, onto) {
        Some(Place::Group(group)) => Some(group),
        Some(Place::Item { group, .. }) => group,
        None => None,
    };
    change(window, live, |tree| tree.move_to(place, to));
}

// --- Opening and lighting ---------------------------------------------------

/// Open the bookmark on a row: its file in the pane the writer is in, the view
/// brought to its heading, and the heading's section lit.
pub(crate) fn activate(window: &AppWindow, live: &Live, row: i32) {
    let Some(bookmark) = place_of_row(window, live, row)
        .and_then(|place| read(live, |tree, _| tree.bookmark(place).cloned()).flatten())
    else {
        return;
    };
    let_go(window, &live.cache, &live.states);
    open_path_in_focused_pane(window, live, &bookmark.path, Opening::Peeked);
    let id = focused_pane(window);
    let document = live.states.document(id);
    let showing = document.file.borrow().path() == Some(bookmark.path.as_path());
    if !showing {
        // Opening said why it could not.
        publish(window, live);
        return;
    }
    let source = document.text.borrow().clone();
    let headings = document::outline(&source);
    let (start, end) = if bookmark.heading.is_empty() {
        // The whole file: nothing to light, but the row stays chosen while the
        // file is in front.
        (0, 0)
    } else if let Some(index) = bookmarks::find_heading(&headings, &bookmark.heading) {
        let start = headings[index].at;
        (
            start,
            bookmarks::section_end(&headings, index, source.len()),
        )
    } else {
        window.tell_tab(
            say!(
                "見出しが見つかりません: {}",
                "Heading not found: {}",
                bookmark.heading.join(" / ")
            )
            .into(),
        );
        publish(window, live);
        return;
    };
    live.cache.borrow_mut().bookmark_mark = Some(BookmarkMark {
        bookmark,
        pane: id,
        document: Rc::downgrade(&document),
        changed_at: document.text.changed_at(),
        start,
        end,
    });
    show_source_range(window, live, id, &source, start, start);
    publish(window, live);
    restore_editor_focus(window);
}

/// The list's empty space was clicked: nothing is chosen, nothing is lit.
pub(crate) fn clear(window: &AppWindow, live: &Live) {
    let_go(window, &live.cache, &live.states);
    publish(window, live);
}

/// Let go of the lit section, drawing its pane again without it. True when
/// there was one.
pub(crate) fn let_go(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    states: &PaneStates,
) -> bool {
    let Some(mark) = cache.borrow_mut().bookmark_mark.take() else {
        return false;
    };
    window.set_bookmark_selected(-1);
    let id = mark.pane;
    let present = id.is_panel() || (id.index() as usize) < states.slots.borrow().len();
    if present {
        let document = states.document(id);
        let state = states.of(id);
        let source = document.text.borrow().clone();
        refresh_pane_from_state(window, cache, &document, id, &state, &source);
    }
    true
}

/// Escape in a pane with nothing else to let go of lets go of the lit section
/// in it.
pub(crate) fn let_go_in(
    window: &AppWindow,
    cache: &Rc<RefCell<RenderCache>>,
    states: &PaneStates,
    id: PaneId,
) -> bool {
    let here = cache
        .borrow()
        .bookmark_mark
        .as_ref()
        .is_some_and(|mark| mark.pane == id && mark.start < mark.end);
    here && let_go(window, cache, states)
}
