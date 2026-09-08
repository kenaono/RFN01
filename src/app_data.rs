//! The editor's own area on disk, and the work copies kept in it.
//!
//! 要件 8.1 asks that edited text survive the application closing or being
//! killed, without the original file being touched, and 要件 5.1 forbids the
//! editor writing anything of its own inside the working folder. Together those
//! leave one place: an area of the editor's own, outside the folder the writer
//! opened.
//!
//! A work copy is one document's unsaved text plus enough to put it back where
//! it came from. It is written whole through [`file_io::write_atomically`], so
//! a copy is never half a document — the case it exists for is the one where
//! the process stops without warning.
//!
//! Only [`app_directory`] knows about Windows, and only by reading an
//! environment variable. Everything else takes the directory as an argument, so
//! the format and the naming can be tested against a directory of the test's
//! own.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::file_io;

/// The editor's folder inside the user's local application data.
const APP_FOLDER: &str = "RFN Edit";

/// Work copies get a folder of their own, so that settings and session state
/// can sit beside them later without anything having to be sorted out by name.
const WORK_FOLDER: &str = "work";

const WORK_EXTENSION: &str = "rfnwork";

/// First line of every work copy.
///
/// A version number from the start: this file outlives the run that wrote it,
/// so the run that reads it may not be the same build.
const WORK_MAGIC: &str = "RFN-EDIT-WORK 1";

/// The name of the file that holds the session (要件 8.5).
///
/// Beside the work copies rather than among them: a work copy is one document's
/// text and is read by looking at every file in the folder, while this is one
/// file about the arrangement.
const SESSION_FILE: &str = "session.rfnsession";
/// 要件 9: the display settings, which are the app's rather than any
/// document's. Beside the session and never inside a document — the
/// requirement is explicit that nothing of this goes into the Markdown.
const SETTINGS_FILE: &str = "settings.rfnsettings";
const SETTINGS_MAGIC: &str = "RFN-EDIT-SETTINGS 1";
/// First line of it, for the same reason the work copies have one.
const SESSION_MAGIC: &str = "RFN-EDIT-SESSION 1";

/// The one quick draft (要件 12.4).
///
/// **One file, not a folder.** 要件 12.4 keeps one draft and says a history is
/// not part of the initial version; a folder would be the shape of a history
/// with nothing in it.
const DRAFT_FILE: &str = "draft.rfndraft";
const DRAFT_MAGIC: &str = "RFN-EDIT-DRAFT 1";

/// The drafts that have been sent somewhere and cleared.
const DRAFT_HISTORY_FILE: &str = "draft-history.rfndrafts";
const DRAFT_HISTORY_MAGIC: &str = "RFN-EDIT-DRAFT-HISTORY 1";

/// How many of them are kept.
///
/// **Ten, and the oldest falls off.** The window clears itself when it is
/// closed and when its text is sent, so this is what stands between the writer
/// and a message they meant to keep — but a list nobody can read to the bottom
/// is a drawer, not a history.
pub const DRAFT_HISTORY_LIMIT: usize = 10;

/// One tab, as the session remembers it.
///
/// The document is named the same way a work copy names one — by its file, or by
/// its untitled number — so the two are matched up when they come back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionTab {
    pub origin: Option<PathBuf>,
    pub untitled: u32,
    /// The four modes of 要件 7.2, kept per tab.
    pub vertical: bool,
    pub preview: bool,
    /// Where the pane was looking, along the flow.
    ///
    /// **In pixels, and therefore only a hint**: the same number means a
    /// different place at another width, zoom or mode, and the window settles
    /// into all three after this is applied (要件 8.5). `top` is what actually
    /// puts the view back.
    pub scroll: i32,
    /// The source byte at the near edge of the view, which is what the writer
    /// was looking at.
    ///
    /// **A place in the text rather than on the screen.** A pixel offset stops
    /// meaning anything the moment the layout is a different size; a byte is
    /// the same passage however the text is set.
    pub top: Option<usize>,
    pub caret: Option<usize>,
    pub anchor: Option<usize>,
    /// 追加要件 Terminal: whether the strip along the foot of the pane was open
    /// while this tab was in front, and how tall the writer had dragged it.
    ///
    /// **The shell itself is not remembered** — it ended when the editor did.
    /// What comes back is the arrangement: the strip opens again, with a shell
    /// started when the tab comes to the front.
    pub below: bool,
    pub below_height: i32,
    /// 追加要件 2026-09-07: whether this tab was still asking what it is.
    ///
    /// **Written only when it was**, so a session from a build without this
    /// reads back exactly as it did — and a tab that had become something is
    /// restored as that thing rather than as the question it started as.
    pub empty: bool,
}

/// One pane's strip, as the session remembers it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPane {
    pub tabs: Vec<SessionTab>,
    pub active: usize,
    /// 要件 9: how far this pane was magnified, as a percentage.
    ///
    /// **With the arrangement rather than with the display settings** — it is
    /// what the writer was doing, not how they like the editor set. Zero means
    /// a session that did not say, which is every session written before the
    /// zoom belonged to a pane; the window puts its own default there.
    pub zoom: i32,
}

/// What was on screen when the editor was last closed (要件 8.5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    /// The layout tree, in `pane_layout::Layout`'s own words. Kept as text here
    /// because this module is about files, not about how panes are arranged.
    pub layout: String,
    /// Where the window was and how big (要件 8.5).
    ///
    /// **The screen is part of the arrangement.** A writer who put the window
    /// on half of one monitor left it there on purpose, and opening in the
    /// middle of the other one is the editor undoing that every morning.
    pub place: Option<WindowPlace>,
    /// Whether it was left filling the screen. Kept beside the place rather
    /// than instead of it: unmaximising has to put the window back somewhere,
    /// and that somewhere is where it was before.
    pub maximized: bool,
    pub focused: i32,
    pub panes: Vec<SessionPane>,
    /// The work folder, and the folders inside it the writer had open
    /// (要件 5.1, 8.5).
    pub folder: Option<PathBuf>,
    /// Which folder the full-text search walks, when it is not the whole work
    /// folder (要件 7.7、2026-09-07追加). **`None` means the work folder**,
    /// rather than a copy of it — the two would drift apart the moment another
    /// folder is opened.
    pub search_folder: Option<PathBuf>,
    pub expanded: Vec<PathBuf>,
    pub tree_shown: bool,
    /// How wide the left pane is, in pixels (追加要件 2026-09-06). **Zero when
    /// nothing said**, which the window reads as its own default — the same
    /// rule the zoom uses, and for the same reason: a pane 0 wide is not a
    /// width anybody chose.
    pub tree_width: u32,
    /// The files opened most recently, newest first (要件 7.7). Kept with the
    /// session rather than with the work folder, because it is a list of what
    /// the writer did and not of what the folder holds.
    pub recent: Vec<PathBuf>,
    /// The work folders opened most recently, newest first (要件 5.1), the one
    /// open now at the head.
    ///
    /// **`folder` above is where the writer is; this is where they have been.**
    /// A window holds one work folder at a time, so the way back to the last
    /// one is a list kept for them rather than a second folder kept open.
    pub folders: Vec<PathBuf>,
}

/// Write the session down.
///
/// One line per fact, in the same shape as a work copy: a name, a colon and a
/// value. A pane begins a new strip and every tab after it belongs to that pane.
pub fn encode_session(session: &Session) -> String {
    let mut out = String::new();
    out.push_str(SESSION_MAGIC);
    out.push('\n');
    out.push_str(&format!("layout: {}\n", session.layout));
    if let Some(place) = session.place {
        let WindowPlace {
            x,
            y,
            width,
            height,
        } = place;
        out.push_str(&format!("place: {x} {y} {width} {height}\n"));
    }
    if session.maximized {
        out.push_str("maximized: 1\n");
    }
    out.push_str(&format!("focused: {}\n", session.focused));
    out.push_str(&format!("tree: {}\n", u8::from(session.tree_shown)));
    out.push_str(&format!("tree-width: {}\n", session.tree_width));
    if let Some(folder) = &session.folder {
        out.push_str(&format!("folder: {}\n", folder.display()));
    }
    if let Some(folder) = &session.search_folder {
        out.push_str(&format!("search: {}\n", folder.display()));
    }
    for open in &session.expanded {
        out.push_str(&format!("expanded: {}\n", open.display()));
    }
    for path in &session.recent {
        out.push_str(&format!("recent: {}\n", path.display()));
    }
    for path in &session.folders {
        out.push_str(&format!("visited: {}\n", path.display()));
    }
    for pane in &session.panes {
        out.push_str(&format!("pane: {} {}\n", pane.active, pane.zoom));
        for tab in &pane.tabs {
            out.push_str(&format!(
                "tab: {} {} {} {}\n",
                tab.untitled,
                u8::from(tab.vertical),
                u8::from(tab.preview),
                tab.scroll,
            ));
            if let Some(origin) = &tab.origin {
                out.push_str(&format!("origin: {}\n", origin.display()));
            }
            if let Some(caret) = tab.caret {
                out.push_str(&format!("caret: {caret}\n"));
            }
            if let Some(anchor) = tab.anchor {
                out.push_str(&format!("anchor: {anchor}\n"));
            }
            if let Some(top) = tab.top {
                out.push_str(&format!("top: {top}\n"));
            }
            if tab.empty {
                out.push_str("empty: 1\n");
            }
            if tab.below || tab.below_height > 0 {
                out.push_str(&format!(
                    "below: {} {}\n",
                    u8::from(tab.below),
                    tab.below_height
                ));
            }
        }
    }
    out
}

/// Read a session back.
///
/// `None` for a file this build cannot make sense of. **Never a reason to
/// refuse to start**: the caller opens the way it would have with no session at
/// all, which is what a first run does.
pub fn decode_session(raw: &str) -> Option<Session> {
    let mut lines = raw.split('\n');
    if lines.next()? != SESSION_MAGIC {
        return None;
    }
    let mut session = Session::default();
    // 要件 9: a session written while the zoom was the window's names one
    // number before any pane. It stands in for every pane that does not name
    // its own, so a writer who was working at 140% opens there rather than at
    // 100 the first time they run a build that magnifies each pane.
    let mut window_zoom = 0;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(": ")?;
        match key {
            "layout" => session.layout = value.to_owned(),
            "place" => session.place = decode_place(value),
            "maximized" => session.maximized = value == "1",
            "focused" => session.focused = value.parse().ok()?,
            "zoom" => window_zoom = value.parse().ok()?,
            "tree" => session.tree_shown = value == "1",
            "tree-width" => session.tree_width = value.parse().unwrap_or(0),
            "folder" => session.folder = Some(PathBuf::from(value)),
            "search" => session.search_folder = Some(PathBuf::from(value)),
            "expanded" => session.expanded.push(PathBuf::from(value)),
            "recent" => session.recent.push(PathBuf::from(value)),
            "visited" => session.folders.push(PathBuf::from(value)),
            "pane" => {
                let mut fields = value.split(' ');
                session.panes.push(SessionPane {
                    tabs: Vec::new(),
                    active: fields.next()?.parse().ok()?,
                    // Absent in a session from before the zoom was the pane's.
                    zoom: fields
                        .next()
                        .and_then(|zoom| zoom.parse().ok())
                        .unwrap_or(window_zoom),
                });
            }
            "tab" => {
                let pane = session.panes.last_mut()?;
                let mut fields = value.split(' ');
                pane.tabs.push(SessionTab {
                    untitled: fields.next()?.parse().ok()?,
                    vertical: fields.next()? == "1",
                    preview: fields.next()? == "1",
                    scroll: fields.next()?.parse().ok()?,
                    ..SessionTab::default()
                });
            }
            // These belong to the tab above them, which is the only place they
            // can be read from.
            "origin" => {
                let tab = session.panes.last_mut()?.tabs.last_mut()?;
                tab.origin = Some(PathBuf::from(value));
            }
            "caret" => {
                let tab = session.panes.last_mut()?.tabs.last_mut()?;
                tab.caret = value.parse().ok();
            }
            "top" => {
                let tab = session.panes.last_mut()?.tabs.last_mut()?;
                tab.top = value.parse().ok();
            }
            "anchor" => {
                let tab = session.panes.last_mut()?.tabs.last_mut()?;
                tab.anchor = value.parse().ok();
            }
            "empty" => {
                let tab = session.panes.last_mut()?.tabs.last_mut()?;
                tab.empty = value == "1";
            }
            "below" => {
                let tab = session.panes.last_mut()?.tabs.last_mut()?;
                let mut fields = value.split(' ');
                tab.below = fields.next() == Some("1");
                tab.below_height = fields.next().and_then(|it| it.parse().ok()).unwrap_or(0);
            }
            // A field this build does not know is from a later one, and the
            // rest of the arrangement is still worth having.
            _ => {}
        }
    }
    Some(session)
}

/// The display settings as names and values (要件 9).
///
/// **This module does not know what any of them mean.** A name and a value is
/// all a file needs; which names exist, what they do and what a missing one
/// falls back to belong to the window. That is also what makes the file
/// forgiving in both directions: a name this build does not know is skipped,
/// and one it knows but does not find keeps whatever the default is — so a
/// settings file written by another build still opens.
pub fn encode_settings(values: &[(String, String)]) -> String {
    let mut out = String::new();
    out.push_str(SETTINGS_MAGIC);
    out.push('\n');
    for (name, value) in values {
        out.push_str(&format!("{name}: {value}\n"));
    }
    out
}

/// The settings a file holds, in the order they were written.
pub fn decode_settings(raw: &str) -> Option<Vec<(String, String)>> {
    let mut lines = raw.split('\n');
    if lines.next()? != SETTINGS_MAGIC {
        return None;
    }
    let mut values = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // A line without the separator is not a setting. **Skipped rather than
        // refused**: one unreadable line is not a reason to open with every
        // colour and size back at its default.
        if let Some((name, value)) = line.split_once(": ") {
            values.push((name.to_owned(), value.to_owned()));
        }
    }
    Some(values)
}

/// Put the display settings away where the next run will look for them.
pub fn write_settings(directory: &Path, values: &[(String, String)]) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(SETTINGS_FILE);
    file_io::write_atomically(&path, encode_settings(values).as_bytes())?;
    Ok(path)
}

/// What the last run was set to, if anything readable.
pub fn read_settings(directory: &Path) -> Option<Vec<(String, String)>> {
    let raw = fs::read_to_string(directory.join(SETTINGS_FILE)).ok()?;
    decode_settings(&raw)
}

/// Put the session away where the next run will look for it.
pub fn write_session(directory: &Path, session: &Session) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(SESSION_FILE);
    file_io::write_atomically(&path, encode_session(session).as_bytes())?;
    Ok(path)
}

/// What the last run left, if anything readable.
pub fn read_session(directory: &Path) -> Option<Session> {
    let raw = fs::read_to_string(directory.join(SESSION_FILE)).ok()?;
    decode_session(&raw)
}

/// One document's unsaved text, and what it takes to put it back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkCopy {
    /// The file being edited, when there is one.
    pub origin: Option<PathBuf>,
    /// Which untitled buffer this is, when there is no file (要件 8.4).
    pub untitled: u32,
    /// Where the caret was, so a restored document opens where it was left.
    pub caret: Option<usize>,
    /// What the origin file was when this copy was taken (要件 8.3、2026-09-08).
    ///
    /// **これが無いと、閉じているあいだの外部変更を見逃す。**復元は元ファイル
    /// を読み直すので、そのとき記録される「合意した姿」は*いまの*ファイルに
    /// なる——別のアプリが書き換えていても`external_change`は「変わっていない」
    /// と答え、`Ctrl+S`が要件 8.3 の問いを出さずに上書きする。退避した時点の
    /// 姿を持ち歩けば、復元したその瞬間から食い違いが見える。
    ///
    /// `None`は「この版より前に書かれたコピー」と「ファイルを持たない文書」の
    /// 両方——どちらも比べる相手がいないので、同じ扱いでよい。
    pub stamp: Option<file_io::FileStamp>,
    pub text: String,
}

/// The stamp as one line of a work copy: 秒・ナノ秒・長さ。
///
/// **時刻が無いことも書く**（`-`）。ファイルシステムが更新時刻を答えなかった
/// ときで、長さだけは比べられる。
fn write_stamp(stamp: &file_io::FileStamp) -> String {
    let since = stamp
        .modified
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok());
    let when = match since {
        Some(since) => format!("{} {}", since.as_secs(), since.subsec_nanos()),
        None => "- -".to_owned(),
    };
    format!("{when} {}", stamp.length)
}

fn read_stamp(written: &str) -> Option<file_io::FileStamp> {
    let mut parts = written.split(' ');
    let secs = parts.next()?;
    let nanos = parts.next()?;
    let length = parts.next()?.parse().ok()?;
    let modified = match (secs, nanos) {
        ("-", _) | (_, "-") => None,
        (secs, nanos) => {
            let since = Duration::new(secs.parse().ok()?, nanos.parse().ok()?);
            Some(UNIX_EPOCH.checked_add(since)?)
        }
    };
    Some(file_io::FileStamp { modified, length })
}

/// The quick draft, and what it takes to open its window where it was
/// (要件 12.4).
///
/// **The text is the point and the rest is convenience**, which is why the two
/// are separated the way `WorkCopy` separates them: header lines that may be
/// missing or unreadable, then the text, taken verbatim from after the blank
/// line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub text: String,
    /// Where the caret was, so the window opens where it was left.
    pub caret: Option<usize>,
    /// 要件 12.2: whether the window stays above the others.
    pub on_top: bool,
    /// The tab the draft was last sent to, as the editor names it (要件 12.4).
    ///
    /// **Opaque here.** What a tab is, and how one is recognised again in the
    /// next run, is the editor's business; this file is where the answer is
    /// kept, not where it is understood. Empty when the draft has never been
    /// sent anywhere.
    pub target: String,
    /// Where the window was and how big, in physical pixels. `None` before the
    /// writer has ever moved it, which is when the window manager decides.
    pub place: Option<WindowPlace>,
}

/// A window's place on the screen, as Windows counts it (要件 8.5, 12.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowPlace {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The draft as it is written: header lines, a blank line, then the text.
pub fn encode_draft(draft: &Draft) -> String {
    let mut out = String::with_capacity(draft.text.len() + 128);
    out.push_str(DRAFT_MAGIC);
    out.push('\n');
    if let Some(caret) = draft.caret {
        out.push_str(&format!("caret: {caret}\n"));
    }
    if draft.on_top {
        out.push_str("on-top: 1\n");
    }
    // A target with a newline in it would be two header lines and a file that
    // no longer parses. Nothing the editor writes has one; refusing to write it
    // is what keeps that true whatever it starts naming tabs by.
    if !draft.target.is_empty() && !draft.target.contains('\n') {
        out.push_str(&format!("target: {}\n", draft.target));
    }
    if let Some(place) = draft.place {
        let WindowPlace {
            x,
            y,
            width,
            height,
        } = place;
        out.push_str(&format!("place: {x} {y} {width} {height}\n"));
    }
    out.push('\n');
    out.push_str(&draft.text);
    out
}

/// A draft read back, or `None` when this is not one.
pub fn decode_draft(raw: &str) -> Option<Draft> {
    let mut lines = raw.split('\n');
    if lines.next()? != DRAFT_MAGIC {
        return None;
    }
    let mut draft = Draft::default();
    let mut consumed = DRAFT_MAGIC.len() + 1;
    for line in lines {
        consumed += line.len() + 1;
        if line.is_empty() {
            draft.text = raw.get(consumed..)?.to_owned();
            return Some(draft);
        }
        let (key, value) = line.split_once(": ")?;
        match key {
            "caret" => draft.caret = value.parse().ok(),
            "on-top" => draft.on_top = value == "1",
            "target" => draft.target = value.to_owned(),
            // **A place that does not parse is no place at all**, rather than a
            // window put at half of one: the writer gets the window manager's
            // choice, which is what they had before they ever moved it.
            "place" => draft.place = decode_place(value),
            // A field this build does not know is from a later one, and the
            // text is the part worth having.
            _ => {}
        }
    }
    None
}

fn decode_place(value: &str) -> Option<WindowPlace> {
    let mut numbers = value.split(' ');
    let place = WindowPlace {
        x: numbers.next()?.parse().ok()?,
        y: numbers.next()?.parse().ok()?,
        width: numbers.next()?.parse().ok()?,
        height: numbers.next()?.parse().ok()?,
    };
    // A window with no size is one nobody can find. **Refused here rather than
    // guarded at every reader**, which is the same rule the session follows.
    (place.width > 0 && place.height > 0).then_some(place)
}

/// Put the draft away where the next run will look for it (要件 12.4).
pub fn write_draft(directory: &Path, draft: &Draft) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(DRAFT_FILE);
    file_io::write_atomically(&path, encode_draft(draft).as_bytes())?;
    Ok(path)
}

/// What the last run left in the draft, if anything readable.
pub fn read_draft(directory: &Path) -> Option<Draft> {
    let raw = fs::read_to_string(directory.join(DRAFT_FILE)).ok()?;
    decode_draft(&raw)
}

/// The drafts kept behind the current one, newest first.
///
/// **Lengths rather than separators.** A draft may hold any line at all,
/// including whatever separator would have been chosen — the same problem the
/// work copies solve with a blank line, and one entry per file is not on offer
/// here. Each entry says how many bytes it is and those bytes follow.
pub fn encode_history(entries: &[String]) -> String {
    let mut out = String::new();
    out.push_str(DRAFT_HISTORY_MAGIC);
    out.push('\n');
    for entry in entries.iter().take(DRAFT_HISTORY_LIMIT) {
        out.push_str(&format!("entry: {}\n", entry.len()));
        out.push_str(entry);
        out.push('\n');
    }
    out
}

/// The history read back, or `None` when this is not one.
pub fn decode_history(raw: &str) -> Option<Vec<String>> {
    let head = raw.strip_prefix(DRAFT_HISTORY_MAGIC)?.strip_prefix('\n')?;
    let mut at = raw.len() - head.len();
    let mut entries = Vec::new();
    while at < raw.len() {
        let rest = raw.get(at..)?;
        let (line, _) = rest.split_once('\n')?;
        let length: usize = line.strip_prefix("entry: ")?.parse().ok()?;
        let start = at + line.len() + 1;
        let end = start.checked_add(length)?;
        // **Refused rather than trimmed.** A length that runs past the end is a
        // file that was written by something else or cut short, and half a
        // draft restored is worse than none.
        let entry = raw.get(start..end)?;
        entries.push(entry.to_owned());
        at = end + 1;
    }
    Some(entries)
}

/// Put a draft at the head of the history (要件 12.4).
///
/// **The same text twice is one entry.** A writer who sends the same message
/// again has not written two drafts, and ten places is few enough that a
/// repeat would push something they wanted off the end.
pub fn remember_draft(entries: &mut Vec<String>, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    entries.retain(|kept| kept != text);
    entries.insert(0, text.to_owned());
    entries.truncate(DRAFT_HISTORY_LIMIT);
}

/// Put the history away where the next run will look for it.
pub fn write_history(directory: &Path, entries: &[String]) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(DRAFT_HISTORY_FILE);
    file_io::write_atomically(&path, encode_history(entries).as_bytes())?;
    Ok(path)
}

/// What the last run left behind the draft. Empty when there is nothing
/// readable — a history is a convenience and must not be able to stop the
/// window opening.
pub fn read_history(directory: &Path) -> Vec<String> {
    fs::read_to_string(directory.join(DRAFT_HISTORY_FILE))
        .ok()
        .and_then(|raw| decode_history(&raw))
        .unwrap_or_default()
}

/// The editor's own area, or `None` when Windows does not say where it is.
pub fn app_directory() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join(APP_FOLDER))
}

/// Where work copies go.
pub fn work_directory() -> Option<PathBuf> {
    Some(app_directory()?.join(WORK_FOLDER))
}

/// FNV-1a over the path's text.
///
/// Not a security question: the name only has to be the same for the same file
/// and different for different ones, and short enough to be a file name. Lower
/// cased first, because two Windows paths differing only in case are one file.
fn path_key(path: &Path) -> u64 {
    let text = path.to_string_lossy().to_lowercase();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The file a document's work copy goes in.
///
/// Stable across runs, because the copy has to be found again after a restart.
/// A saved file is named by its path — hashed, so the name is short and holds
/// no character a file name cannot; an untitled buffer is named by its number.
pub fn work_file_name(copy: &WorkCopy) -> String {
    match &copy.origin {
        Some(path) => {
            let key = path_key(path);
            format!("file-{key:016x}.{WORK_EXTENSION}")
        }
        None => {
            let number = copy.untitled;
            format!("untitled-{number}.{WORK_EXTENSION}")
        }
    }
}

/// The work copy as it is written.
///
/// Header lines, a blank line, then the text taken verbatim. The blank line is
/// the whole of the parsing rule, which is what lets a document contain lines
/// that look exactly like the header without any escaping.
pub fn encode(copy: &WorkCopy) -> String {
    let mut out = String::with_capacity(copy.text.len() + 128);
    out.push_str(WORK_MAGIC);
    out.push('\n');
    out.push_str(&format!("untitled: {}\n", copy.untitled));
    if let Some(origin) = &copy.origin {
        out.push_str(&format!("origin: {}\n", origin.display()));
    }
    if let Some(caret) = copy.caret {
        out.push_str(&format!("caret: {caret}\n"));
    }
    if let Some(stamp) = &copy.stamp {
        out.push_str(&format!("stamp: {}\n", write_stamp(stamp)));
    }
    out.push('\n');
    out.push_str(&copy.text);
    out
}

/// A work copy read back, or `None` when this is not one.
///
/// Refused rather than guessed at, for the same reason `file_io` refuses bytes
/// that are not UTF-8: a half-understood work copy would be restored as damage.
pub fn decode(raw: &str) -> Option<WorkCopy> {
    let mut lines = raw.split('\n');
    if lines.next()? != WORK_MAGIC {
        return None;
    }
    let mut copy = WorkCopy::default();
    let mut consumed = WORK_MAGIC.len() + 1;
    for line in lines {
        consumed += line.len() + 1;
        if line.is_empty() {
            copy.text = raw.get(consumed..)?.to_owned();
            return Some(copy);
        }
        let (key, value) = line.split_once(": ")?;
        match key {
            "untitled" => copy.untitled = value.parse().ok()?,
            "origin" => copy.origin = Some(PathBuf::from(value)),
            "caret" => copy.caret = value.parse().ok(),
            // **読めなければ`None`のまま**——「比べる相手を持たない」に倒れる
            // ので、古いコピーと同じ扱いになる。
            "stamp" => copy.stamp = read_stamp(value),
            // A field this build does not know is from a later one. Ignored
            // rather than refused: the text is the part worth having.
            _ => {}
        }
    }
    None
}

/// Write one document's work copy, replacing any earlier one for it.
pub fn write_into(directory: &Path, copy: &WorkCopy) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(work_file_name(copy));
    file_io::write_atomically(&path, encode(copy).as_bytes())?;
    Ok(path)
}

/// Every work copy in the directory.
///
/// Anything unreadable or unrecognised is skipped rather than reported: this
/// runs at start-up, and one bad file must not stop the others being restored.
pub fn read_all_in(directory: &Path) -> Vec<WorkCopy> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut copies = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new(WORK_EXTENSION)) {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(copy) = decode(&raw) {
            copies.push(copy);
        }
    }
    copies
}

/// Drop a work copy that is no longer needed (要件 8.2).
///
/// A copy that is already gone is not an error: the point of the call is that
/// it should not be there afterwards.
pub fn discard_in(directory: &Path, copy: &WorkCopy) -> io::Result<()> {
    let path = directory.join(work_file_name(copy));
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_directory(name: &str) -> PathBuf {
        let temporary = std::env::temp_dir();
        let directory = temporary.join(format!("rfnedit-work-{name}"));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("creates");
        directory
    }

    fn session() -> Session {
        Session {
            layout: "S h 0.4000 P 0 P 1".to_owned(),
            place: Some(WindowPlace {
                x: -8,
                y: 120,
                width: 1180,
                height: 760,
            }),
            maximized: false,
            focused: 1,
            folder: Some(PathBuf::from("D:\\書きかけ")),
            search_folder: Some(PathBuf::from("D:\\書きかけ\\章")),
            expanded: vec![PathBuf::from("D:\\書きかけ\\章")],
            tree_shown: true,
            tree_width: 260,
            recent: vec![
                PathBuf::from("D:\\書きかけ\\第一章.md"),
                PathBuf::from("D:\\書きかけ\\年表.txt"),
            ],
            folders: vec![PathBuf::from("D:\\書きかけ"), PathBuf::from("D:\\古い原稿")],
            panes: vec![
                SessionPane {
                    active: 1,
                    zoom: 125,
                    tabs: vec![
                        SessionTab {
                            untitled: 2,
                            vertical: true,
                            scroll: -4200,
                            top: Some(1200),
                            caret: Some(17),
                            anchor: Some(3),
                            ..SessionTab::default()
                        },
                        SessionTab {
                            origin: Some(PathBuf::from("D:\\書きかけ\\第一章.md")),
                            preview: true,
                            ..SessionTab::default()
                        },
                    ],
                },
                SessionPane {
                    active: 0,
                    zoom: 80,
                    tabs: vec![SessionTab::default()],
                },
            ],
        }
    }

    /// 要件 8.5: the arrangement, the strips and each tab's mode come back.
    #[test]
    fn the_settings_round_trip() {
        let values = vec![
            ("body-size".to_owned(), "22".to_owned()),
            ("h1".to_owned(), "200".to_owned()),
            ("ink".to_owned(), "#24211e".to_owned()),
        ];
        let written = encode_settings(&values);

        assert_eq!(decode_settings(&written), Some(values));
    }

    /// **A name this build does not know is not an error.** The file is read by
    /// whatever build the writer happens to be running, and one that has lost a
    /// setting or gained one still opens with everything else in place.
    #[test]
    fn an_unknown_setting_is_carried_rather_than_refused() {
        let raw = format!("{SETTINGS_MAGIC}\nbody-size: 22\nweather: 晴れ\nnot a setting\n");

        let read = decode_settings(&raw).expect("reads");

        assert_eq!(read.len(), 2);
        assert_eq!(read[0], ("body-size".to_owned(), "22".to_owned()));
        // The line with no separator is dropped; the one with an unknown name
        // is kept, because only the window knows which names mean anything.
        assert_eq!(read[1].0, "weather");
    }

    #[test]
    fn settings_written_by_something_else_are_refused() {
        assert_eq!(decode_settings("hello\nbody-size: 22\n"), None);
    }

    #[test]
    fn a_session_round_trips() {
        let written = encode_session(&session());

        assert_eq!(decode_session(&written), Some(session()));
    }

    /// A tab's own fields belong to the tab above them, however many panes
    /// there are.
    #[test]
    fn a_tab_keeps_the_pane_it_was_written_under() {
        let read = decode_session(&encode_session(&session())).expect("reads");

        assert_eq!(read.panes.len(), 2);
        assert_eq!(read.panes[0].tabs.len(), 2);
        let second = read.panes[0].tabs[1].origin.as_deref();
        assert_eq!(second, Some(Path::new("D:\\書きかけ\\第一章.md")));
        assert_eq!(read.panes[1].tabs[0].caret, None);
    }

    /// A file this build cannot make sense of is not a reason to refuse to
    /// start: the caller opens the way a first run does.
    #[test]
    fn an_unreadable_session_is_refused_rather_than_guessed_at() {
        assert_eq!(decode_session("something else entirely"), None);
        assert_eq!(decode_session(""), None);
        // A field from a later build is skipped, and the rest still reads.
        let later = format!("{SESSION_MAGIC}\nlayout: P 0\nweather: 晴れ\n");
        let read = decode_session(&later).expect("reads");
        assert_eq!(read.layout, "P 0");
        // A session written before there was a history has none, rather than
        // being unreadable for want of one. The same holds for the folders
        // visited (要件 5.1), which arrived later still.
        assert!(read.recent.is_empty());
        assert!(read.folders.is_empty());
    }

    /// 要件 9: the zoom is each pane's own, and a session written while it was
    /// the window's still opens — its one number stands in for every pane.
    #[test]
    fn a_pane_keeps_its_own_zoom() {
        let read = decode_session(&encode_session(&session())).expect("reads");

        assert_eq!(read.panes[0].zoom, 125);
        assert_eq!(read.panes[1].zoom, 80);

        let older = format!("{SESSION_MAGIC}\nlayout: P 0\nzoom: 140\npane: 0\npane: 1\n");
        let read = decode_session(&older).expect("reads");
        assert_eq!(read.panes[0].zoom, 140);
        assert_eq!(read.panes[1].zoom, 140);

        // And one from before there was a zoom at all says nothing, which the
        // window reads as its own default rather than as 0%.
        let oldest = format!("{SESSION_MAGIC}\nlayout: P 0\npane: 0\n");
        let read = decode_session(&oldest).expect("reads");
        assert_eq!(read.panes[0].zoom, 0);
    }

    /// What is written is what comes back, through the file.
    #[test]
    fn a_session_survives_the_disk() {
        let directory = scratch_directory("session");
        write_session(&directory, &session()).expect("writes");

        assert_eq!(read_session(&directory), Some(session()));
    }

    fn untitled_copy(text: &str) -> WorkCopy {
        WorkCopy {
            origin: None,
            untitled: 1,
            caret: Some(3),
            stamp: None,
            text: text.to_owned(),
        }
    }

    /// 追加要件 Terminal: **the strip is part of the arrangement too.**
    ///
    /// The shell in it is not — it ended with the editor — so what comes back
    /// is that it was open, and how tall.
    #[test]
    fn a_session_remembers_the_strip_along_the_foot() {
        let mut opened = session();
        if let Some(pane) = opened.panes.first_mut() {
            if let Some(tab) = pane.tabs.first_mut() {
                tab.below = true;
                tab.below_height = 260;
            }
        }
        let read = decode_session(&encode_session(&opened)).expect("decodes");
        let tab = &read.panes[0].tabs[0];
        assert!(tab.below);
        assert_eq!(tab.below_height, 260);

        // **A session written before this build says nothing about it**, and a
        // strip nobody asked for is a strip that stays shut.
        let read = decode_session(&encode_session(&session())).expect("decodes");
        assert!(!read.panes[0].tabs[0].below);
        assert_eq!(read.panes[0].tabs[0].below_height, 0);
    }

    /// 要件 8.5: **the screen is part of the arrangement.** A window put on
    /// half of one monitor was put there on purpose.
    #[test]
    fn a_session_remembers_where_the_window_was() {
        let read = decode_session(&encode_session(&session())).expect("decodes");

        assert_eq!(read.place, session().place);
        assert!(!read.maximized);

        // Maximised is kept beside the place, not instead of it: a window that
        // is un-maximised has to go back somewhere.
        let filled = Session {
            maximized: true,
            ..session()
        };
        let read = decode_session(&encode_session(&filled)).expect("decodes");
        assert!(read.maximized);
        assert_eq!(read.place, filled.place);

        // A session from a build that wrote no place opens where the window
        // manager puts it, which is what every run did before this.
        let older = Session {
            place: None,
            ..session()
        };
        let read = decode_session(&encode_session(&older)).expect("decodes");
        assert_eq!(read.place, None);
    }

    /// 要件 12.4: the kept drafts come back as they were, whatever is in them.
    ///
    /// **Including a draft that looks like the file it is stored in.** The
    /// entries are given by length, so nothing a writer types can be mistaken
    /// for the end of one.
    #[test]
    fn the_draft_history_round_trips() {
        let entries = vec![
            "いちばん新しい下書き\n二行目\n".to_owned(),
            "entry: 4\nRFN-EDIT-DRAFT-HISTORY 1\n".to_owned(),
            "".to_owned(),
        ];

        let read = decode_history(&encode_history(&entries)).expect("decodes");

        assert_eq!(read, entries);
    }

    /// **The newest is first, the same text is one entry, and ten is the
    /// most.** A writer who sends the same message twice has not written two
    /// drafts, and a repeat would otherwise push out something they wanted.
    #[test]
    fn a_remembered_draft_goes_to_the_head_of_the_list() {
        let mut entries = Vec::new();
        remember_draft(&mut entries, "ひとつめ");
        remember_draft(&mut entries, "ふたつめ");
        remember_draft(&mut entries, "ひとつめ");

        assert_eq!(entries, vec!["ひとつめ", "ふたつめ"]);

        // Nothing is not a draft, however it is spelled.
        remember_draft(&mut entries, "");
        remember_draft(&mut entries, "  \n\t");
        assert_eq!(entries.len(), 2);

        for number in 0..DRAFT_HISTORY_LIMIT {
            remember_draft(&mut entries, &format!("下書き{number}"));
        }
        assert_eq!(entries.len(), DRAFT_HISTORY_LIMIT);
        assert_eq!(entries[0], format!("下書き{}", DRAFT_HISTORY_LIMIT - 1));
    }

    /// **A length that runs past the end is refused**, rather than restored as
    /// half a draft.
    #[test]
    fn refuses_a_history_that_was_cut_short() {
        assert!(decode_history("RFN-EDIT-DRAFT-HISTORY 1\nentry: 40\n短い\n").is_none());
        assert!(decode_history("下書き\n").is_none());
        assert_eq!(
            decode_history("RFN-EDIT-DRAFT-HISTORY 1\n").expect("an empty history is a history"),
            Vec::<String>::new()
        );
    }

    /// 要件 12.4: the draft comes back as it was left — the text, the caret,
    /// the window and whether it stays on top.
    #[test]
    fn a_draft_round_trips() {
        let draft = Draft {
            text: "下書きです\n二行目\n".to_owned(),
            caret: Some(9),
            on_top: true,
            target: "file:D:\\note.md".to_owned(),
            place: Some(WindowPlace {
                x: -40,
                y: 120,
                width: 460,
                height: 340,
            }),
        };

        let read = decode_draft(&encode_draft(&draft)).expect("decodes");

        assert_eq!(read, draft);
    }

    /// The blank line is the whole of the parsing rule here too, so a draft may
    /// hold lines that look exactly like the header — which a draft written in
    /// this editor's own notation certainly will.
    #[test]
    fn a_draft_may_contain_lines_that_look_like_the_header() {
        let draft = Draft {
            text: "caret: 4\non-top: 1\n\nplace: 1 2 3 4\n".to_owned(),
            ..Draft::default()
        };

        let read = decode_draft(&encode_draft(&draft)).expect("decodes");

        assert_eq!(read.text, draft.text);
        assert_eq!(read.caret, None);
        assert!(!read.on_top);
        assert_eq!(read.place, None);
    }

    /// An empty draft is a draft: 要件 12.4 says the window opens on what was
    /// left in it, and nothing is what a cleared one leaves.
    #[test]
    fn an_empty_draft_round_trips() {
        let read = decode_draft(&encode_draft(&Draft::default())).expect("decodes");

        assert_eq!(read, Draft::default());
    }

    /// **A place that does not parse is no place**, rather than a window put at
    /// half of one. The text is still the part worth having.
    #[test]
    fn a_broken_place_leaves_the_window_where_windows_puts_it() {
        let raw = "RFN-EDIT-DRAFT 1\nplace: 10 20\ncaret: 2\n\n下書き";

        let read = decode_draft(raw).expect("decodes");

        assert_eq!(read.place, None);
        assert_eq!(read.caret, Some(2));
        assert_eq!(read.text, "下書き");
    }

    /// A window of no size is one nobody can find.
    #[test]
    fn a_place_with_no_size_is_refused() {
        let raw = "RFN-EDIT-DRAFT 1\nplace: 10 20 0 340\n\n下書き";

        assert_eq!(decode_draft(raw).expect("decodes").place, None);
    }

    #[test]
    fn refuses_something_that_is_not_a_draft() {
        assert!(decode_draft("下書き\n").is_none());
        // A work copy is not a draft, however much it looks like one.
        assert!(decode_draft(&encode(&untitled_copy("本文"))).is_none());
    }

    #[test]
    fn a_work_copy_round_trips() {
        let copy = untitled_copy("本文\n二行目\n");
        let read = decode(&encode(&copy)).expect("decodes");
        assert_eq!(read, copy);
    }

    /// The reason the header ends at a blank line rather than at a count.
    #[test]
    fn a_document_may_contain_lines_that_look_like_the_header() {
        let copy = untitled_copy("origin: not a field\n\nuntitled: 9\n");
        let read = decode(&encode(&copy)).expect("decodes");
        assert_eq!(read.text, copy.text);
        assert_eq!(read.origin, None);
        assert_eq!(read.untitled, 1);
    }

    #[test]
    fn an_empty_document_round_trips() {
        let copy = untitled_copy("");
        let read = decode(&encode(&copy)).expect("decodes");
        assert_eq!(read.text, "");
    }

    #[test]
    fn a_file_copy_keeps_its_origin_and_caret() {
        let copy = WorkCopy {
            origin: Some(PathBuf::from("D:\\原稿\\note.md")),
            untitled: 0,
            caret: Some(120),
            stamp: None,
            text: "本文".to_owned(),
        };
        let read = decode(&encode(&copy)).expect("decodes");
        assert_eq!(read, copy);
    }

    /// 要件 8.3（2026-09-08）: **どの版に対して書いていたかが往復する。**
    /// これが往復しないと、復元した文書は「閉じているあいだの外部変更」を
    /// 見分けられない。
    #[test]
    fn a_copy_remembers_the_file_it_was_taken_against() {
        let stamp = file_io::FileStamp {
            modified: Some(UNIX_EPOCH + Duration::new(1_757_000_000, 123_456_700)),
            length: 4096,
        };
        let copy = WorkCopy {
            origin: Some(PathBuf::from("D:\\原稿\\note.md")),
            untitled: 0,
            caret: Some(1),
            stamp: Some(stamp),
            text: "本文".to_owned(),
        };
        let read = decode(&encode(&copy)).expect("decodes");

        assert_eq!(read, copy);
        assert_eq!(read.stamp.expect("stamp").modified, stamp.modified);
    }

    /// 更新時刻を答えないファイルシステムもある。**長さだけは比べられる**ので、
    /// 時刻が無いことも書いて往復させる。
    #[test]
    fn a_copy_without_a_time_still_carries_the_length() {
        let stamp = file_io::FileStamp {
            modified: None,
            length: 7,
        };
        let written = write_stamp(&stamp);

        assert_eq!(written, "- - 7");
        assert_eq!(read_stamp(&written), Some(stamp));
    }

    /// この版より前に書かれたコピーには`stamp:`の行が無い。**読めなければ
    /// `None`**——比べる相手を持たない、という同じ答えに倒れる。
    #[test]
    fn a_copy_from_an_older_build_has_no_stamp() {
        let older = format!("{WORK_MAGIC}\nuntitled: 2\ncaret: 0\n\n本文");
        let read = decode(&older).expect("decodes");

        assert_eq!(read.stamp, None);
        assert_eq!(read.text, "本文");
    }

    #[test]
    fn refuses_something_that_is_not_a_work_copy() {
        assert!(decode("# ただのMarkdown\n").is_none());
    }

    /// The name has to be the same next time, or the copy is never found.
    #[test]
    fn the_same_file_always_gets_the_same_name() {
        let copy = WorkCopy {
            origin: Some(PathBuf::from("D:\\原稿\\note.md")),
            ..WorkCopy::default()
        };
        assert_eq!(work_file_name(&copy), work_file_name(&copy.clone()));
    }

    /// Two Windows paths differing only in case are one file.
    #[test]
    fn a_path_names_the_same_copy_whatever_its_case() {
        let lower = WorkCopy {
            origin: Some(PathBuf::from("d:\\notes\\a.md")),
            ..WorkCopy::default()
        };
        let upper = WorkCopy {
            origin: Some(PathBuf::from("D:\\Notes\\A.md")),
            ..WorkCopy::default()
        };
        assert_eq!(work_file_name(&lower), work_file_name(&upper));
    }

    #[test]
    fn different_files_get_different_names() {
        let first = WorkCopy {
            origin: Some(PathBuf::from("d:\\notes\\a.md")),
            ..WorkCopy::default()
        };
        let second = WorkCopy {
            origin: Some(PathBuf::from("d:\\notes\\b.md")),
            ..WorkCopy::default()
        };
        assert_ne!(work_file_name(&first), work_file_name(&second));
    }

    #[test]
    fn untitled_buffers_are_named_by_their_number() {
        let first = WorkCopy {
            untitled: 1,
            ..WorkCopy::default()
        };
        let second = WorkCopy {
            untitled: 2,
            ..WorkCopy::default()
        };
        assert_ne!(work_file_name(&first), work_file_name(&second));
    }

    #[test]
    fn writes_and_reads_a_work_copy_back() {
        let directory = scratch_directory("round-trip");
        let copy = untitled_copy("退避された本文\n");
        write_into(&directory, &copy).expect("writes");
        let read = read_all_in(&directory);
        assert_eq!(read, vec![copy]);
        let _ = fs::remove_dir_all(&directory);
    }

    /// Writing again must replace the copy, not add another.
    #[test]
    fn a_second_write_replaces_the_first() {
        let directory = scratch_directory("replace");
        write_into(&directory, &untitled_copy("一度目")).expect("writes");
        write_into(&directory, &untitled_copy("二度目")).expect("writes");
        let read = read_all_in(&directory);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].text, "二度目");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn discarding_removes_the_copy() {
        let directory = scratch_directory("discard");
        let copy = untitled_copy("本文");
        write_into(&directory, &copy).expect("writes");
        discard_in(&directory, &copy).expect("discards");
        assert!(read_all_in(&directory).is_empty());
        // Twice, because a save may follow a discard that already happened.
        discard_in(&directory, &copy).expect("discards again");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_directory_that_does_not_exist_has_no_copies() {
        let directory = scratch_directory("missing");
        let _ = fs::remove_dir_all(&directory);
        assert!(read_all_in(&directory).is_empty());
    }

    /// One unreadable file must not hide the others.
    #[test]
    fn a_file_that_is_not_a_work_copy_is_skipped() {
        let directory = scratch_directory("mixed");
        let copy = untitled_copy("本文");
        write_into(&directory, &copy).expect("writes");
        let stray = directory.join("stray.rfnwork");
        fs::write(&stray, "not a work copy").expect("writes");
        let read = read_all_in(&directory);
        assert_eq!(read, vec![copy]);
        let _ = fs::remove_dir_all(&directory);
    }
}
