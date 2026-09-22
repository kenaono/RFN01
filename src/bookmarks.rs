//! Bookmarks and the outline's folds (書き手の求め 2026-09-22).
//!
//! A Workspace keeps one tree of bookmarks: bookmarks at its root, and groups
//! one level deep that hold bookmarks of their own. A bookmark names a file
//! and, when it was made on a heading, the chain of headings down to it — the
//! heading is found again by what the headings say, not by a byte that an edit
//! would move.
//!
//! **No Slint here.** The tree, its order, its filter, its file and the
//! arithmetic of a heading's range are plain data; `main.rs` connects them to
//! the window.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::document::Heading;
use crate::file_io;
use crate::link_completion;
use crate::workspace::WorkspaceId;

/// One bookmark: what the list calls it, the file, and the heading in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub title: String,
    pub path: PathBuf,
    /// The headings from the top of the document down to the one bookmarked,
    /// each by what it says. **Empty for the whole file** — a document with no
    /// heading at the caret is still something to come back to.
    pub heading: Vec<String>,
}

/// A group: a folder of bookmarks, one level deep and no deeper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub open: bool,
    pub items: Vec<Bookmark>,
}

/// A Workspace's whole tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub root: Vec<Bookmark>,
    pub groups: Vec<Group>,
    /// Titles run Z to A rather than A to Z.
    pub descending: bool,
}

/// Where something is in a [`Tree`], by position in what is stored — never by
/// its place in the list on screen, which the order and the filter rearrange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Group(usize),
    Item { group: Option<usize>, index: usize },
}

/// One row of the list as it is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub place: Place,
    pub title: String,
    pub depth: i32,
    pub group: bool,
    pub open: bool,
}

/// Why a name was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    Taken,
}

/// Case-folded first, so that "b" does not come after "Z"; the text itself
/// decides a tie, so the order never depends on the order things were added.
fn by_title(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

impl Tree {
    /// The rows to draw: groups first, then the bookmarks at the root, each
    /// run in title order. `filter` keeps the bookmarks whose title holds it
    /// (ignoring case) and the groups that hold one of them — or whose own
    /// name holds it, in which case all they hold is shown. **A group is drawn
    /// open while filtering**, because a match hidden in a closed group is not
    /// found.
    pub fn rows(&self, filter: &str) -> Vec<Row> {
        let needle = filter.trim().to_lowercase();
        let hit = |text: &str| needle.is_empty() || text.to_lowercase().contains(&needle);
        let mut rows = Vec::new();
        for group in self.ordered(self.groups.iter().map(|group| group.name.as_str())) {
            let held = &self.groups[group];
            let name_hit = !needle.is_empty() && hit(&held.name);
            let titles = held.items.iter().map(|item| item.title.as_str());
            let items = self
                .ordered(titles)
                .into_iter()
                .filter(|index| name_hit || hit(&held.items[*index].title))
                .collect::<Vec<_>>();
            if !needle.is_empty() && !name_hit && items.is_empty() {
                continue;
            }
            let open = held.open || !needle.is_empty();
            rows.push(Row {
                place: Place::Group(group),
                title: held.name.clone(),
                depth: 0,
                group: true,
                open,
            });
            if open {
                for index in items {
                    rows.push(Row {
                        place: Place::Item {
                            group: Some(group),
                            index,
                        },
                        title: held.items[index].title.clone(),
                        depth: 1,
                        group: false,
                        open: false,
                    });
                }
            }
        }
        let titles = self.root.iter().map(|item| item.title.as_str());
        for index in self.ordered(titles) {
            if !hit(&self.root[index].title) {
                continue;
            }
            rows.push(Row {
                place: Place::Item { group: None, index },
                title: self.root[index].title.clone(),
                depth: 0,
                group: false,
                open: false,
            });
        }
        rows
    }

    /// Positions of `titles`, in the order the list shows them.
    fn ordered<'a>(&self, titles: impl Iterator<Item = &'a str>) -> Vec<usize> {
        let titles = titles.collect::<Vec<_>>();
        let mut order = (0..titles.len()).collect::<Vec<_>>();
        order.sort_by(|a, b| {
            let ordering = by_title(titles[*a], titles[*b]);
            if self.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
        order
    }

    /// The group names, in the order the list shows them, with where each is
    /// stored — what the dialog's group menu and "Move to" offer.
    pub fn group_choices(&self) -> Vec<(usize, String)> {
        self.ordered(self.groups.iter().map(|group| group.name.as_str()))
            .into_iter()
            .map(|index| (index, self.groups[index].name.clone()))
            .collect()
    }

    pub fn bookmark(&self, place: Place) -> Option<&Bookmark> {
        match place {
            Place::Group(_) => None,
            Place::Item { group: None, index } => self.root.get(index),
            Place::Item {
                group: Some(group),
                index,
            } => self.groups.get(group)?.items.get(index),
        }
    }

    /// Where `bookmark` is stored, if it still is.
    pub fn find(&self, bookmark: &Bookmark) -> Option<Place> {
        if let Some(index) = self.root.iter().position(|item| item == bookmark) {
            return Some(Place::Item { group: None, index });
        }
        self.groups.iter().enumerate().find_map(|(group, held)| {
            let index = held.items.iter().position(|item| item == bookmark)?;
            Some(Place::Item {
                group: Some(group),
                index,
            })
        })
    }

    /// Put `bookmark` in a group, or at the root. A group that is not there
    /// means the root — the list may have changed while the dialog was open.
    pub fn add(&mut self, bookmark: Bookmark, group: Option<usize>) -> Place {
        match group.filter(|group| *group < self.groups.len()) {
            Some(group) => {
                self.groups[group].items.push(bookmark);
                let index = self.groups[group].items.len() - 1;
                Place::Item {
                    group: Some(group),
                    index,
                }
            }
            None => {
                self.root.push(bookmark);
                let index = self.root.len() - 1;
                Place::Item { group: None, index }
            }
        }
    }

    fn check_group_name(&self, name: &str, except: Option<usize>) -> Result<(), NameError> {
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        let folded = name.to_lowercase();
        let taken = self
            .groups
            .iter()
            .enumerate()
            .any(|(index, group)| Some(index) != except && group.name.to_lowercase() == folded);
        if taken { Err(NameError::Taken) } else { Ok(()) }
    }

    /// A new, empty group. **Two groups may not share a name** (ignoring
    /// case): the dialog and "Move to" pick one by what it is called.
    pub fn add_group(&mut self, name: &str) -> Result<usize, NameError> {
        let name = name.trim();
        self.check_group_name(name, None)?;
        self.groups.push(Group {
            name: name.to_owned(),
            open: true,
            items: Vec::new(),
        });
        Ok(self.groups.len() - 1)
    }

    pub fn rename(&mut self, place: Place, name: &str) -> Result<(), NameError> {
        let name = name.trim();
        match place {
            Place::Group(group) => {
                self.check_group_name(name, Some(group))?;
                if let Some(held) = self.groups.get_mut(group) {
                    held.name = name.to_owned();
                }
            }
            Place::Item { group, index } => {
                if name.is_empty() {
                    return Err(NameError::Empty);
                }
                let items = match group {
                    None => Some(&mut self.root),
                    Some(group) => self.groups.get_mut(group).map(|held| &mut held.items),
                };
                if let Some(item) = items.and_then(|items| items.get_mut(index)) {
                    item.title = name.to_owned();
                }
            }
        }
        Ok(())
    }

    /// Take something out. A group goes with everything in it.
    pub fn remove(&mut self, place: Place) -> Option<Bookmark> {
        match place {
            Place::Group(group) => {
                if group < self.groups.len() {
                    self.groups.remove(group);
                }
                None
            }
            Place::Item { group: None, index } => {
                (index < self.root.len()).then(|| self.root.remove(index))
            }
            Place::Item {
                group: Some(group),
                index,
            } => {
                let items = &mut self.groups.get_mut(group)?.items;
                (index < items.len()).then(|| items.remove(index))
            }
        }
    }

    /// Move a bookmark into a group, or to the root. Moving it where it already
    /// is changes nothing.
    pub fn move_to(&mut self, place: Place, to: Option<usize>) -> Option<Place> {
        let Place::Item { group, .. } = place else {
            return None;
        };
        if group == to || to.is_some_and(|to| to >= self.groups.len()) {
            return Some(place);
        }
        let bookmark = self.remove(place)?;
        Some(self.add(bookmark, to))
    }

    pub fn toggle(&mut self, group: usize) {
        if let Some(held) = self.groups.get_mut(group) {
            held.open = !held.open;
        }
    }
}

/// `path` from inside `root`, written the way links are (forward slashes).
///
/// **Compared as written, not as `Path`s**: a registered root is canonical —
/// `\\?\D:\…` on Windows — and a document's path usually is not, so
/// `strip_prefix` would never match. The drive letter's case does not matter.
fn relative_to(path: &str, root: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    let head = path.get(..root.len())?;
    let rest = path.get(root.len()..)?.strip_prefix('/')?;
    head.eq_ignore_ascii_case(root).then(|| rest.to_owned())
}

/// What the dialog shows as the link: the file, from the Workspace folder it
/// is in when it is in one, and the heading chain after a `#`.
pub fn link_label(path: &Path, heading: &[String], roots: &[PathBuf]) -> String {
    let written = link_completion::path_to_string(path);
    let file = roots
        .iter()
        .find_map(|root| relative_to(&written, &link_completion::path_to_string(root)))
        .unwrap_or(written);
    if heading.is_empty() {
        file
    } else {
        format!("{file} # {}", heading.join(" / "))
    }
}

// --- Headings -----------------------------------------------------------

/// The chain of headings down to `index`: each heading's nearest ancestor of
/// a smaller level, from the top.
pub fn heading_path(headings: &[Heading], index: usize) -> Vec<String> {
    let Some(target) = headings.get(index) else {
        return Vec::new();
    };
    let mut chain = vec![target.text.clone()];
    let mut level = target.level;
    for heading in headings[..index].iter().rev() {
        if heading.level < level {
            chain.push(heading.text.clone());
            level = heading.level;
        }
    }
    chain.reverse();
    chain
}

/// Every heading's chain, in one pass.
fn heading_paths(headings: &[Heading]) -> Vec<Vec<String>> {
    let mut stack: Vec<(u8, String)> = Vec::new();
    headings
        .iter()
        .map(|heading| {
            while stack
                .last()
                .is_some_and(|(level, _)| *level >= heading.level)
            {
                stack.pop();
            }
            stack.push((heading.level, heading.text.clone()));
            stack.iter().map(|(_, text)| text.clone()).collect()
        })
        .collect()
}

/// Find a bookmarked heading again. The whole chain first; failing that — a
/// parent renamed, the section moved under another — the first heading that
/// says the same as the one bookmarked.
pub fn find_heading(headings: &[Heading], chain: &[String]) -> Option<usize> {
    let last = chain.last()?;
    heading_paths(headings)
        .iter()
        .position(|path| path == chain)
        .or_else(|| headings.iter().position(|heading| &heading.text == last))
}

/// Where the section under `index` ends: the next heading at the same level
/// or above, or the end of the text.
pub fn section_end(headings: &[Heading], index: usize, length: usize) -> usize {
    let Some(heading) = headings.get(index) else {
        return length;
    };
    headings[index + 1..]
        .iter()
        .find(|next| next.level <= heading.level)
        .map_or(length, |next| next.at)
}

/// The heading whose section holds `byte` — the last one that starts at or
/// before it.
pub fn heading_at(headings: &[Heading], byte: usize) -> Option<usize> {
    headings.iter().rposition(|heading| heading.at <= byte)
}

/// One row of the folded outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutlineRow {
    /// Which heading of the whole outline this is.
    pub heading: usize,
    /// Whether it has headings under it, and so a fold.
    pub parent: bool,
    pub open: bool,
}

/// The headings the outline shows when the ones in `folded` (by chain) are
/// closed: everything under a closed heading is left out.
pub fn outline_rows(headings: &[Heading], folded: &HashSet<Vec<String>>) -> Vec<OutlineRow> {
    let paths = heading_paths(headings);
    let mut hidden_under: Option<u8> = None;
    let mut rows = Vec::new();
    for (index, heading) in headings.iter().enumerate() {
        if let Some(level) = hidden_under {
            if heading.level > level {
                continue;
            }
            hidden_under = None;
        }
        let parent = headings
            .get(index + 1)
            .is_some_and(|next| next.level > heading.level);
        let open = !(parent && folded.contains(&paths[index]));
        if !open {
            hidden_under = Some(heading.level);
        }
        rows.push(OutlineRow {
            heading: index,
            parent,
            open,
        });
    }
    rows
}

/// Every heading that has something under it — what "Collapse All" closes.
pub fn foldable(headings: &[Heading]) -> HashSet<Vec<String>> {
    let paths = heading_paths(headings);
    headings
        .iter()
        .enumerate()
        .filter(|(index, heading)| {
            headings
                .get(index + 1)
                .is_some_and(|next| next.level > heading.level)
        })
        .map(|(index, _)| paths[index].clone())
        .collect()
}

// --- The file -----------------------------------------------------------
//
// One file per numeric Workspace id, in the same small length-prefixed shape
// the Workspace ledger and its view state use.

const MAGIC: &str = "RFN-EDIT-BOOKMARKS 1";
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const PREFIX: &str = "workspace-bookmarks-";
const SUFFIX: &str = ".rfnbookmarks";

fn file_for(appdata_dir: &Path, id: WorkspaceId) -> PathBuf {
    appdata_dir.join(format!("{PREFIX}{id}{SUFFIX}"))
}

fn push_text(out: &mut String, key: &str, text: &str) {
    out.push_str(&format!("{key}: {}\n", text.len()));
    out.push_str(text);
    out.push('\n');
}

fn push_bookmark(out: &mut String, bookmark: &Bookmark) {
    push_text(out, "item", &bookmark.title);
    push_text(out, "path", &bookmark.path.to_string_lossy());
    out.push_str(&format!("heading: {}\n", bookmark.heading.len()));
    for part in &bookmark.heading {
        push_text(out, "part", part);
    }
}

pub fn encode(tree: &Tree) -> String {
    let mut out = String::new();
    out.push_str(MAGIC);
    out.push('\n');
    let order = if tree.descending { "desc" } else { "asc" };
    out.push_str(&format!("order: {order}\n"));
    for bookmark in &tree.root {
        push_bookmark(&mut out, bookmark);
    }
    for group in &tree.groups {
        out.push_str(&format!("open: {}\n", u8::from(group.open)));
        push_text(&mut out, "group", &group.name);
        for bookmark in &group.items {
            push_bookmark(&mut out, bookmark);
        }
    }
    out
}

/// Reads what [`encode`] wrote, line by line and length by length.
struct Reader<'a> {
    raw: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn line(&mut self) -> Option<&'a str> {
        let rest = self.raw.get(self.at..)?;
        let (line, _) = rest.split_once('\n')?;
        self.at += line.len() + 1;
        Some(line)
    }

    fn peek_key(&self) -> Option<&'a str> {
        let rest = self.raw.get(self.at..)?;
        let (line, _) = rest.split_once('\n')?;
        line.split_once(": ").map(|(key, _)| key)
    }

    fn value(&mut self, key: &str) -> Option<&'a str> {
        let (found, value) = self.line()?.split_once(": ")?;
        (found == key).then_some(value)
    }

    fn text(&mut self, key: &str) -> Option<&'a str> {
        let length: usize = self.value(key)?.parse().ok()?;
        let end = self.at.checked_add(length)?;
        let text = self.raw.get(self.at..end)?;
        if self.raw.get(end..end + 1)? != "\n" {
            return None;
        }
        self.at = end + 1;
        Some(text)
    }

    fn bookmark(&mut self) -> Option<Bookmark> {
        let title = self.text("item")?.to_owned();
        let path = PathBuf::from(self.text("path")?);
        let count: usize = self.value("heading")?.parse().ok()?;
        let mut heading = Vec::new();
        for _ in 0..count {
            heading.push(self.text("part")?.to_owned());
        }
        Some(Bookmark {
            title,
            path,
            heading,
        })
    }
}

pub fn decode(raw: &str) -> Option<Tree> {
    if raw.len() as u64 > MAX_BYTES {
        return None;
    }
    let mut reader = Reader { raw, at: 0 };
    if reader.line()? != MAGIC {
        return None;
    }
    let descending = match reader.value("order")? {
        "asc" => false,
        "desc" => true,
        _ => return None,
    };
    let mut tree = Tree {
        descending,
        ..Tree::default()
    };
    while reader.at < raw.len() {
        match reader.peek_key()? {
            "item" => {
                let bookmark = reader.bookmark()?;
                match tree.groups.last_mut() {
                    Some(group) => group.items.push(bookmark),
                    None => tree.root.push(bookmark),
                }
            }
            "open" => {
                let open = match reader.value("open")? {
                    "0" => false,
                    "1" => true,
                    _ => return None,
                };
                let name = reader.text("group")?.to_owned();
                tree.groups.push(Group {
                    name,
                    open,
                    items: Vec::new(),
                });
            }
            _ => return None,
        }
    }
    Some(tree)
}

pub fn save(appdata_dir: &Path, id: WorkspaceId, tree: &Tree) -> io::Result<()> {
    let encoded = encode(tree);
    if encoded.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bookmarks exceed the maximum encoded size",
        ));
    }
    fs::create_dir_all(appdata_dir)?;
    file_io::write_atomically(&file_for(appdata_dir, id), encoded.as_bytes())?;
    Ok(())
}

/// A Workspace's tree, or an empty one when nothing is saved or what is saved
/// cannot be read.
pub fn load(appdata_dir: &Path, id: WorkspaceId) -> Tree {
    let Ok(file) = fs::File::open(file_for(appdata_dir, id)) else {
        return Tree::default();
    };
    let mut buffer = Vec::new();
    if file.take(MAX_BYTES + 1).read_to_end(&mut buffer).is_err() {
        return Tree::default();
    }
    String::from_utf8(buffer)
        .ok()
        .and_then(|raw| decode(&raw))
        .unwrap_or_default()
}

/// Drop a Workspace's bookmarks, when the Workspace itself is removed.
pub fn remove(appdata_dir: &Path, id: WorkspaceId) -> io::Result<()> {
    match fs::remove_file(file_for(appdata_dir, id)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Drop every Workspace's bookmarks — when the ledger starts over, the ids
/// are handed out again, and old bookmarks must not attach to a new one.
pub fn remove_all(appdata_dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(appdata_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix(PREFIX)
            .and_then(|body| body.strip_suffix(SUFFIX))
            .is_some_and(|id| id.parse::<u64>().is_ok())
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::outline;

    fn mark(title: &str) -> Bookmark {
        Bookmark {
            title: title.to_owned(),
            path: PathBuf::from(format!("C:\\w\\{title}.md")),
            heading: vec!["一".to_owned(), title.to_owned()],
        }
    }

    fn titles(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| format!("{}{}", "  ".repeat(row.depth as usize), row.title))
            .collect()
    }

    fn sample() -> Tree {
        let mut tree = Tree::default();
        tree.add(mark("beta"), None);
        tree.add(mark("Alpha"), None);
        let notes = tree.add_group("Notes").expect("adds");
        let drafts = tree.add_group("drafts").expect("adds");
        tree.add(mark("zeta"), Some(notes));
        tree.add(mark("eta"), Some(notes));
        tree.add(mark("章"), Some(drafts));
        tree
    }

    #[test]
    fn groups_come_first_and_everything_runs_in_title_order_either_way() {
        let mut tree = sample();
        assert_eq!(
            titles(&tree.rows("")),
            [
                "drafts", "  章", "Notes", "  eta", "  zeta", "Alpha", "beta"
            ]
        );
        tree.descending = true;
        assert_eq!(
            titles(&tree.rows("")),
            [
                "Notes", "  zeta", "  eta", "drafts", "  章", "beta", "Alpha"
            ]
        );
    }

    #[test]
    fn a_closed_group_hides_what_it_holds_until_a_filter_needs_it() {
        let mut tree = sample();
        tree.toggle(0);
        assert_eq!(
            titles(&tree.rows("")),
            ["drafts", "  章", "Notes", "Alpha", "beta"]
        );
        assert_eq!(
            titles(&tree.rows("ET")),
            ["Notes", "  eta", "  zeta", "beta"]
        );
        assert_eq!(titles(&tree.rows("dra")), ["drafts", "  章"]);
        assert_eq!(titles(&tree.rows("nothing")), Vec::<String>::new());
    }

    #[test]
    fn group_names_are_unique_and_never_empty() {
        let mut tree = sample();
        assert_eq!(tree.add_group("notes"), Err(NameError::Taken));
        assert_eq!(tree.add_group("  "), Err(NameError::Empty));
        assert_eq!(
            tree.rename(Place::Group(0), "Drafts"),
            Err(NameError::Taken)
        );
        assert_eq!(tree.rename(Place::Group(0), "NOTES"), Ok(()));
        assert_eq!(tree.groups[0].name, "NOTES");
        let root = Place::Item {
            group: None,
            index: 0,
        };
        assert_eq!(tree.rename(root, ""), Err(NameError::Empty));
        assert_eq!(tree.rename(root, " gamma "), Ok(()));
        assert_eq!(tree.root[0].title, "gamma");
    }

    #[test]
    fn moving_and_removing_keep_the_bookmark_itself() {
        let mut tree = sample();
        let beta = mark("beta");
        let place = tree.find(&beta).expect("stored");
        let moved = tree.move_to(place, Some(1)).expect("moves");
        assert_eq!(tree.bookmark(moved), Some(&beta));
        assert_eq!(tree.find(&beta), Some(moved));
        assert_eq!(tree.root.len(), 1);
        let back = tree.move_to(moved, None).expect("moves");
        assert_eq!(
            back,
            Place::Item {
                group: None,
                index: 1
            }
        );
        assert_eq!(tree.move_to(back, Some(9)), Some(back));
        tree.remove(Place::Group(0));
        assert_eq!(tree.groups.len(), 1);
        assert_eq!(tree.find(&mark("zeta")), None);
        assert_eq!(tree.remove(back), Some(beta));
    }

    #[test]
    fn the_file_reads_back_what_was_written() {
        let mut tree = sample();
        tree.descending = true;
        tree.toggle(1);
        tree.add(
            Bookmark {
                title: "line\nbreak: 3".to_owned(),
                path: PathBuf::from("C:\\w\\whole.txt"),
                heading: Vec::new(),
            },
            None,
        );
        let encoded = encode(&tree);
        assert_eq!(decode(&encoded), Some(tree));
        assert_eq!(decode("RFN-EDIT-BOOKMARKS 2\norder: asc\n"), None);
        assert_eq!(decode(&encoded[..encoded.len() - 1]), None);
        assert_eq!(
            decode("RFN-EDIT-BOOKMARKS 1\norder: asc\n"),
            Some(Tree::default())
        );
    }

    #[test]
    fn saving_is_per_workspace_and_starting_over_drops_them_all() {
        let directory = std::env::temp_dir().join("rfnedit-bookmarks-save");
        let _ = fs::remove_dir_all(&directory);
        let tree = sample();
        save(&directory, 3, &tree).expect("saves");
        save(&directory, 4, &Tree::default()).expect("saves");
        assert_eq!(load(&directory, 3), tree);
        assert_eq!(load(&directory, 5), Tree::default());
        remove(&directory, 4).expect("removes");
        remove(&directory, 4).expect("absent is fine");
        remove_all(&directory).expect("clears");
        assert_eq!(load(&directory, 3), Tree::default());
        let _ = fs::remove_dir_all(&directory);
    }

    const TEXT: &str = "# 一\nintro\n## 二\nbody\n### 三\ndeep\n## 四\nmore\n# 五\nend\n";

    #[test]
    fn a_heading_is_found_again_by_its_chain_and_its_section_runs_to_the_next_peer() {
        let headings = outline(TEXT);
        assert_eq!(heading_path(&headings, 2), ["一", "二", "三"]);
        let chain = vec!["一".to_owned(), "四".to_owned()];
        assert_eq!(find_heading(&headings, &chain), Some(3));
        let moved = vec!["五".to_owned(), "三".to_owned()];
        assert_eq!(find_heading(&headings, &moved), Some(2));
        assert_eq!(find_heading(&headings, &["無".to_owned()]), None);
        let two = &headings[1];
        assert_eq!(
            &TEXT[two.at..section_end(&headings, 1, TEXT.len())],
            "## 二\nbody\n### 三\ndeep\n"
        );
        assert_eq!(
            &TEXT[headings[4].at..section_end(&headings, 4, TEXT.len())],
            "# 五\nend\n"
        );
        assert_eq!(heading_at(&headings, 0), Some(0));
        assert_eq!(heading_at(&headings, TEXT.find("deep").unwrap()), Some(2));
        assert_eq!(heading_at(&outline("no heading\n"), 3), None);
    }

    #[test]
    fn a_folded_heading_hides_everything_under_it_and_nothing_else() {
        let headings = outline(TEXT);
        let shown = |folded: &HashSet<Vec<String>>| {
            outline_rows(&headings, folded)
                .iter()
                .map(|row| (row.heading, row.parent, row.open))
                .collect::<Vec<_>>()
        };
        let none = HashSet::new();
        assert_eq!(
            shown(&none),
            [
                (0, true, true),
                (1, true, true),
                (2, false, true),
                (3, false, true),
                (4, false, true)
            ]
        );
        let two = HashSet::from([heading_path(&headings, 1)]);
        assert_eq!(
            shown(&two),
            [
                (0, true, true),
                (1, true, false),
                (3, false, true),
                (4, false, true)
            ]
        );
        let all = foldable(&headings);
        assert_eq!(all.len(), 2);
        assert_eq!(shown(&all), [(0, true, false), (4, false, true)]);
    }

    #[test]
    fn the_link_names_the_file_from_its_workspace_folder() {
        let roots = [PathBuf::from(r"\\?\C:\w")];
        let heading = ["一".to_owned(), "二".to_owned()];
        let inside = Path::new(r"c:\w\章\a.md");
        assert_eq!(link_label(inside, &heading, &roots), "章/a.md # 一 / 二");
        let outside = Path::new(r"D:\x.md");
        assert_eq!(link_label(outside, &[], &roots), "D:/x.md");
        let beside = Path::new(r"C:\wide\x.md");
        assert_eq!(link_label(beside, &[], &roots), "C:/wide/x.md");
    }
}
