//! Phase 2 of Workspace設計.md: pure data for link completion.
//!
//! Detecting a trigger in typed text and turning [`crate::workspace_index::Entry`]
//! values into candidates a caller could show in a list — nothing here reads
//! a file, touches Slint, or inserts anything into a document. Whether a
//! caret sits inside a code fence or mid-IME-composition is the caller's own
//! question to ask (of [`crate::document`], which already answers it for the
//! editor's own purposes); this module only hands back where a trigger
//! started, so that question has something to ask it about.
//!
//! # Escaping (Codex review, round 2)
//!
//! `document::link_here`'s two grammars are both naive, first-match splits:
//! `[[note]]` cuts at the *first* `]]` and the *first* `|`; `[shown](target)`
//! cuts at the *first* `)`. Wrapping a target in `<...>` does **not** protect
//! against any of that, because `link_here` finds its delimiter before the
//! resolver ever sees the text. The only thing that actually survives both
//! grammars is not writing the delimiter byte at all: [`percent_encode_reserved`]
//! substitutes `%XX` for the specific ASCII bytes each grammar treats
//! specially, leaving every other byte — all of Japanese included — untouched,
//! and [`percent_decode`] is its inverse, which `workspace_links::resolve_link`
//! (and the picture resolver) apply exactly once before a `#` is read as the
//! heading separator.

use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::document;
use crate::workspace_index::Entry;

/// Candidates are cut off here regardless of how many would otherwise match,
/// and regardless of what a caller asks for — [`candidates`] clamps to this
/// even if given a larger `limit`.
pub const DEFAULT_CANDIDATE_LIMIT: usize = 100;

/// What kind of link is being typed, and — for a heading — which file it
/// names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerKind {
    /// `[[` not yet followed by `#` or a closing `]]`.
    WikiFile,
    /// `[[file#` — `file` is exactly the text between `[[` and `#`, empty for
    /// `[[#`, which asks for the *current* document's own headings.
    WikiHeading { file: String },
    /// `[shown](` not yet followed by `#` or closed by `)`.
    MarkdownFile,
    /// `[shown](file#` — the same file/heading split as [`WikiHeading`], for
    /// an ordinary Markdown link instead of a wiki one.
    MarkdownHeading { file: String },
}

impl TriggerKind {
    pub(crate) fn is_wiki(&self) -> bool {
        matches!(
            self,
            TriggerKind::WikiFile | TriggerKind::WikiHeading { .. }
        )
    }

    pub(crate) fn is_heading(&self) -> bool {
        matches!(
            self,
            TriggerKind::WikiHeading { .. } | TriggerKind::MarkdownHeading { .. }
        )
    }
}

/// Where a trigger began in the source, and what has been typed since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub kind: TriggerKind,
    /// **画像の記法の中か**（RFN01-48）。`![[`の`!`、または`![説明](`の`!`が
    /// 付いているとき。画像の候補は画像だけなので、絞り込みに使う。
    pub image: bool,
    /// Byte offset of the trigger's own marker (`[[` or `(`). A caller can
    /// hand this to `document::line_style` (or similar) to decide the caret
    /// is inside a fence or a code span and reject the whole context — this
    /// module never makes that call itself.
    pub trigger_at: usize,
    /// Byte offset where the typed query starts — the beginning of what a
    /// chosen candidate replaces.
    pub query_at: usize,
    /// `source[query_at..caret]` at the moment this [`Context`] was built.
    pub query: String,
    /// Whether the target already ends *after* `caret` — at this trigger's
    /// own closing delimiter (`]]` or `)`), or at the `|` a wiki link uses to
    /// start its shown text. A candidate's `insert` leaves the closer off
    /// when this is `true`, so accepting one never writes a second `]]`/`)`
    /// next to the one already there, and never cuts an alias off (仕様の実装
    /// 依頼 #11; the alias case 2026-09-21).
    pub already_closed: bool,
    /// Where the target's own text ends, when something already ends it —
    /// see [`Context::already_closed`]. A chosen candidate replaces up to
    /// here rather than up to `caret`, so a caret placed *inside* a written
    /// target replaces the whole of it and leaves whatever follows — an
    /// alias, the closer — untouched (書き手の報告 2026-09-21).
    pub target_end: Option<usize>,
}

/// Whether `range` is a byte range a caller could safely slice `source`
/// with — in bounds, start no later than end, and both ends on a char
/// boundary.
pub fn valid_replacement_range(source: &str, range: Range<usize>) -> bool {
    range.start <= range.end
        && range.end <= source.len()
        && source.is_char_boundary(range.start)
        && source.is_char_boundary(range.end)
}

/// The trigger active at `caret` in `source`, if any — scanning back only to
/// the start of `caret`'s own line, since none of the four shapes this
/// module knows about ever spans a line break.
pub fn detect(source: &str, caret: usize) -> Option<Context> {
    if caret > source.len() || !source.is_char_boundary(caret) {
        return None;
    }
    let line_start = source[..caret].rfind('\n').map_or(0, |at| at + 1);
    let prefix = &source[line_start..caret];

    let wiki_open = prefix
        .rfind("[[")
        .filter(|&open| !prefix[open + 2..].contains("]]"));
    let markdown_open = prefix
        .rfind("](")
        .filter(|&open| !prefix[open + 2..].contains(')'));

    let (relative_trigger, is_wiki) = match (wiki_open, markdown_open) {
        (Some(wiki), Some(markdown)) => {
            if wiki > markdown {
                (wiki, true)
            } else {
                (markdown, false)
            }
        }
        (Some(wiki), None) => (wiki, true),
        (None, Some(markdown)) => (markdown, false),
        (None, None) => return None,
    };
    let trigger_at = line_start + relative_trigger;
    let after_open = &prefix[relative_trigger + 2..];
    let query_at = match after_open.find('#') {
        Some(hash) => trigger_at + 2 + hash + 1,
        None => trigger_at + 2,
    };
    let file = after_open
        .find('#')
        .map(|hash| after_open[..hash].to_owned());
    let closer = if is_wiki { "]]" } else { ")" };
    // **Where the target already ends, not where the caret is.** The caret
    // can sit inside a target that is already written — `[[Target#Target
    // heading]]` with the caret before its own `ing`, say. Replacing only up
    // to the caret left the tail behind and wrote a second closer
    // (`[[Target#Target%20heading%20Test]]ing]]`, 書き手の報告 2026-09-21);
    // ending at the target's own end replaces the whole of it and keeps what
    // follows — an alias, the closer — intact. Only this line is searched, so
    // a `]]` further down the document is never mistaken for this link's.
    let rest_of_line = {
        let rest = &source[caret..];
        let end = rest.find('\n').unwrap_or(rest.len());
        &rest[..end]
    };
    let closer_at = rest_of_line.find(closer).map(|at| caret + at);
    let alias_bar_at = is_wiki
        .then(|| rest_of_line.find('|').map(|at| caret + at))
        .flatten();
    let target_end = [closer_at, alias_bar_at].into_iter().flatten().min();
    let already_closed = target_end.is_some();

    let kind = match (is_wiki, file) {
        (true, Some(file)) => TriggerKind::WikiHeading { file },
        (true, None) => TriggerKind::WikiFile,
        (false, Some(file)) => TriggerKind::MarkdownHeading { file },
        (false, None) => TriggerKind::MarkdownFile,
    };
    // **画像の記法か**（RFN01-48）。`![`はどちらの文法でも記法の頭に付く——
    // `![[`は`[[`の直前、`![説明](`は`]`の手前の`[`の直前にある。文法は
    // `document::link_here`と同じ素朴な読み方なので、入れ子の角括弧は数えない
    // （`[a[b]](x)`のような形はリンクとしても読まれない）。
    let image = if is_wiki {
        trigger_at > 0 && source.as_bytes()[trigger_at - 1] == b'!'
    } else {
        source[..trigger_at]
            .rfind('[')
            .is_some_and(|open| open > 0 && source.as_bytes()[open - 1] == b'!')
    };

    Some(Context {
        kind,
        image,
        trigger_at,
        query_at,
        query: source[query_at..caret].to_owned(),
        already_closed,
        target_end,
    })
}

/// One thing a caller could offer in a completion list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// What this candidate resolves to — the indexed entry's own canonical
    /// path, or (for a same-document heading) the source file if known.
    pub path: PathBuf,
    /// Set only for a heading candidate, and always an actual heading's own
    /// text from `document::outline` — never a guessed slug.
    pub heading: Option<String>,
    /// The heading's own byte offset in its document
    /// ([`crate::document::Heading::at`]), carried alongside `heading` so a
    /// caller can tell two same-titled headings apart by position even
    /// though [`Candidate::insert`] cannot (仕様の実装依頼 #9 — a later
    /// resolver needs this; text alone is ambiguous).
    pub heading_at: Option<usize>,
    /// What a completion list shows. For a file, qualified with its root's
    /// name — or, if that alone would collide with another candidate in the
    /// same list, the root's *full* path — so two files are never confused
    /// (仕様 "同名ファイルはフォルダとパスで区別する"). For a heading that
    /// shares its text with another heading in this same list, an occurrence
    /// marker like `(2/3)` is appended so the list itself never hides the
    /// ambiguity.
    pub display: String,
    /// What replaces `context.query_at..context.target_end.unwrap_or(caret)`
    /// if this candidate is chosen — percent-encoded per
    /// [`percent_encode_reserved`], and closed (`]]` or `)`) unless
    /// [`Context::already_closed`] said one was already there.
    pub insert: String,
    /// Where the caret should land within `insert` after it is written — 直後
    /// of the path or heading text, *before* any closing delimiter this
    /// candidate appended, so typing `#` right away starts a heading query
    /// without having to step back over `]]`/`)` first (仕様の実装依頼 #11).
    pub caret_after_insert: usize,
}

/// Bytes [`document::link_here`]'s `[[...]]` grammar reads specially:
/// `|` splits the shown text off, `]` risks an early `]]`, `#` is this
/// design's own heading separator, and `%` is the escape's own marker.
const WIKI_RESERVED: [u8; 4] = *b"|]#%";
/// The same idea for `(...)`: `)` ends the target early, `#` is the heading
/// separator, `%` is the escape's own marker.
const MARKDOWN_RESERVED: [u8; 3] = *b")#%";

/// Substitutes `%XX` (uppercase hex) for every reserved byte in `text`, plus
/// any ASCII control byte and space — leaving every other byte, Japanese
/// included, exactly as it was. Byte-wise and UTF-8-safe: a multi-byte
/// character's own bytes are always `>= 0x80` and so never match a
/// (necessarily ASCII) entry of `reserved`, never split, never touched.
pub fn percent_encode_reserved(text: &str, reserved: &[u8]) -> String {
    let mut out = Vec::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte < 0x80 && (reserved.contains(&byte) || byte <= 0x20) {
            out.extend_from_slice(format!("%{byte:02X}").as_bytes());
        } else {
            out.push(byte);
        }
    }
    String::from_utf8(out)
        .expect("only ASCII bytes were substituted; multi-byte sequences are untouched")
}

/// The inverse of [`percent_encode_reserved`] — the one decoder every link
/// and picture resolver uses. Any `%` not followed by two valid hex digits is
/// left exactly as written rather than treated as an error: there is no
/// partial-decode failure to report here, only text.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 3 <= bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[at + 1..at + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    at += 3;
                    continue;
                }
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a target at its first *literal* `#` — the file portion's own `#`
/// bytes are percent-encoded by [`percent_encode_reserved`] before this ever
/// sees them, so a bare `#` surviving in the text can only be the separator
/// this design reserves it for.
pub fn split_target_heading(target: &str) -> (&str, Option<&str>) {
    match target.find('#') {
        Some(at) => (&target[..at], Some(&target[at + 1..])),
        None => (target, None),
    }
}

/// Every candidate for `context`, built from `entries` and, for a
/// same-document heading, from `current_source`'s own live outline rather
/// than anything indexed. `limit` is clamped to at most
/// [`DEFAULT_CANDIDATE_LIMIT`]; `0` yields no candidates at all.
pub fn candidates(
    entries: &[Entry],
    source_file: Option<&Path>,
    current_source: &str,
    context: &Context,
    limit: usize,
) -> Vec<Candidate> {
    let limit = limit.min(DEFAULT_CANDIDATE_LIMIT);
    if limit == 0 {
        return Vec::new();
    }
    if context.kind.is_heading() {
        // **画像に見出しは無い**（RFN01-48）。`![[画像.png#`に候補を出さない。
        if context.image {
            return Vec::new();
        }
        let file = match &context.kind {
            TriggerKind::WikiHeading { file } | TriggerKind::MarkdownHeading { file } => {
                file.as_str()
            }
            _ => unreachable!(),
        };
        heading_candidates(
            entries,
            source_file,
            current_source,
            file,
            &context.query,
            context,
            limit,
        )
    } else {
        file_candidates(entries, source_file, context, limit)
    }
}

fn file_candidates(
    entries: &[Entry],
    source_file: Option<&Path>,
    context: &Context,
    limit: usize,
) -> Vec<Candidate> {
    let wiki = context.kind.is_wiki();
    let reserved: &[u8] = if wiki {
        &WIKI_RESERVED
    } else {
        &MARKDOWN_RESERVED
    };
    let closer = if wiki { "]]" } else { ")" };
    let query = context.query.to_lowercase();

    let mut matched: Vec<&Entry> = Vec::new();
    for entry in entries {
        // **画像の記法の中では画像だけ**（RFN01-48）。通常のリンクは今までどおり
        // ——画像も候補に出る（`[[絵.png]]`は開く先として使える）。
        let wanted = if context.image {
            crate::workspace_index::is_image(&entry.canonical)
        } else {
            crate::workspace_index::is_indexable(&entry.canonical)
        };
        if !wanted {
            continue;
        }
        if matched.len() >= limit {
            break;
        }
        let plain_display = format!(
            "{} / {}",
            root_label(entry),
            path_to_string(&entry.relative)
        );
        if !query.is_empty() && !plain_display.to_lowercase().contains(&query) {
            continue;
        }
        matched.push(entry);
    }

    // Disambiguate a root-name collision within *this* list only, by
    // escalating just the colliding candidates to their full root path.
    let mut label_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let plain_displays: Vec<String> = matched
        .iter()
        .map(|entry| {
            format!(
                "{} / {}",
                root_label(entry),
                path_to_string(&entry.relative)
            )
        })
        .collect();
    for display in &plain_displays {
        *label_counts.entry(display.clone()).or_insert(0) += 1;
    }

    matched
        .iter()
        .zip(plain_displays)
        .map(|(entry, plain_display)| {
            let display = if label_counts.get(&plain_display).copied().unwrap_or(0) > 1 {
                format!(
                    "{} / {}",
                    path_to_string(&entry.root),
                    path_to_string(&entry.relative)
                )
            } else {
                plain_display
            };
            let path_text = percent_encode_reserved(
                &path_to_string(&display_path(entry, source_file)),
                reserved,
            );
            let (insert, caret_after_insert) = if context.already_closed {
                (path_text.clone(), path_text.len())
            } else {
                (format!("{path_text}{closer}"), path_text.len())
            };
            Candidate {
                path: entry.canonical.clone(),
                heading: None,
                heading_at: None,
                display,
                insert,
                caret_after_insert,
            }
        })
        .collect()
}

fn heading_candidates(
    entries: &[Entry],
    source_file: Option<&Path>,
    current_source: &str,
    file: &str,
    query: &str,
    context: &Context,
    limit: usize,
) -> Vec<Candidate> {
    let wiki = context.kind.is_wiki();
    let closer = if wiki { "]]" } else { ")" };
    let reserved: &[u8] = if wiki {
        &WIKI_RESERVED
    } else {
        &MARKDOWN_RESERVED
    };

    let (path, headings): (PathBuf, Vec<document::Heading>) = if file.trim().is_empty() {
        (
            source_file.map(Path::to_path_buf).unwrap_or_default(),
            document::outline(current_source),
        )
    } else {
        // Compared decoded-to-decoded, never decoded-to-still-encoded: `file`
        // is what the writer typed (decoded here), but `wiki_or_markdown_path`
        // returns the *encoded* form a candidate's own `insert` uses — a file
        // with a space/`%`/`#` in its name matched nothing until both sides
        // were normalized the same way (Additional reviewed integration
        // decisions, 2026-09-18). A leading `./` is also stripped from both
        // sides — a same-directory candidate's own `insert` now carries one
        // (see `relative_between`), but a hand-typed bare filename never did
        // and must still match.
        let decoded = percent_decode(file);
        let resolved = crate::workspace_links::resolve_indexed_file(
            &decoded,
            wiki,
            source_file,
            entries,
            false,
        )
        .ok();
        let matched = resolved.and_then(|path| entries.iter().find(|e| e.canonical == path));
        match matched {
            Some(entry) => (entry.canonical.clone(), entry.headings.clone()),
            None => return Vec::new(),
        }
    };

    let query = query.to_lowercase();
    let mut occurrences: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for heading in &headings {
        *occurrences.entry(heading.text.as_str()).or_insert(0) += 1;
    }
    let mut seen_so_far: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();

    let mut out = Vec::new();
    for heading in &headings {
        if out.len() >= limit {
            break;
        }
        if !query.is_empty() && !heading.text.to_lowercase().contains(&query) {
            continue;
        }
        let total = *occurrences.get(heading.text.as_str()).unwrap_or(&1);
        let index = {
            let counter = seen_so_far.entry(heading.text.as_str()).or_insert(0);
            *counter += 1;
            *counter
        };
        let display = if total > 1 {
            format!("{} ({index}/{total})", heading.text)
        } else {
            heading.text.clone()
        };
        let text = percent_encode_reserved(&heading.text, reserved);
        // A duplicate heading's own text alone cannot tell two occurrences
        // apart once written into a link, so a literal `#{index}` (1-based)
        // is appended when there is more than one — unambiguous because a
        // literal `#` inside `text` itself is always `%23` by construction
        // (Additional reviewed integration decisions, 2026-09-18).
        let body = if total > 1 {
            format!("{text}#{index}")
        } else {
            text
        };
        let (insert, caret_after_insert) = if context.already_closed {
            (body.clone(), body.len())
        } else {
            (format!("{body}{closer}"), body.len())
        };
        out.push(Candidate {
            path: path.clone(),
            heading: Some(heading.text.clone()),
            heading_at: Some(heading.at),
            display,
            insert,
            caret_after_insert,
        });
    }
    out
}

/// The short name a completion list shows for which root a file is under —
/// its own folder name.
fn root_label(entry: &Entry) -> String {
    entry
        .root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_to_string(&entry.root))
}

/// A path as it is written into a document: forward slashes always, and —
/// on Windows — the `\\?\` extended-length prefix `canonicalize` adds taken
/// back off, since the raw text (`//?/C:/...`) is not a path anything can
/// reopen. A plain UNC path (`\\server\share`) is unaffected, becoming
/// `//server/share` exactly as it already did (Additional reviewed
/// integration decisions, 2026-09-18: "Normalize Windows verbatim prefix").
#[cfg(windows)]
pub(crate) fn path_to_string(path: &Path) -> String {
    use std::path::{Component, Prefix};
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::VerbatimDisk(letter) | Prefix::Disk(letter) => {
                    out.push(letter as char);
                    out.push(':');
                }
                Prefix::VerbatimUNC(server, share) | Prefix::UNC(server, share) => {
                    out.push_str("//");
                    out.push_str(&server.to_string_lossy());
                    out.push('/');
                    out.push_str(&share.to_string_lossy());
                }
                _ => out.push_str(&prefix.as_os_str().to_string_lossy().replace('\\', "/")),
            },
            Component::RootDir => {
                if !out.ends_with('/') {
                    out.push('/');
                }
            }
            Component::CurDir => {
                if out.is_empty() {
                    out.push_str("./");
                }
            }
            Component::ParentDir => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str("..");
            }
            Component::Normal(part) => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(&part.to_string_lossy());
            }
        }
    }
    out
}

/// A path as it is written into a document: forward slashes always.
#[cfg(not(windows))]
pub(crate) fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Encode a known target using the same qualified relative spelling as completion.
pub(crate) fn target_path_text(target: &Path, source_file: Option<&Path>, wiki: bool) -> String {
    let relative = source_file
        .and_then(Path::parent)
        .and_then(|parent| relative_between(parent, target));
    let path = relative.as_deref().unwrap_or(target);
    percent_encode_reserved(
        &path_to_string(path),
        if wiki {
            &WIKI_RESERVED
        } else {
            &MARKDOWN_RESERVED
        },
    )
}

fn display_path(entry: &Entry, source_file: Option<&Path>) -> PathBuf {
    if let Some(source) = source_file {
        if let Some(directory) = source.parent() {
            if let Some(relative) = relative_between(directory, &entry.canonical) {
                return relative;
            }
        }
    }
    entry.canonical.clone()
}

/// Compare Windows namespace prefixes without changing filename case or doing I/O.
fn equivalent_component(a: std::path::Component<'_>, b: std::path::Component<'_>) -> bool {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let (Component::Prefix(a), Component::Prefix(b)) = (a, b) {
            return match (a.kind(), b.kind()) {
                (
                    Prefix::Disk(a) | Prefix::VerbatimDisk(a),
                    Prefix::Disk(b) | Prefix::VerbatimDisk(b),
                ) => a.eq_ignore_ascii_case(&b),
                (
                    Prefix::UNC(a_server, a_share) | Prefix::VerbatimUNC(a_server, a_share),
                    Prefix::UNC(b_server, b_share) | Prefix::VerbatimUNC(b_server, b_share),
                ) => a_server == b_server && a_share == b_share,
                _ => a == b,
            };
        }
    }
    // Preserve the spelling of ordinary components, including case-sensitive directories.
    a == b
}

/// The relative path from `from_dir` to `to`, or `None` for different roots.
fn relative_between(from_dir: &Path, to: &Path) -> Option<PathBuf> {
    let from: Vec<_> = from_dir.components().collect();
    let target: Vec<_> = to.components().collect();
    if !from
        .first()
        .zip(target.first())
        .is_some_and(|(a, b)| equivalent_component(*a, *b))
    {
        return None;
    }
    let common = from
        .iter()
        .zip(target.iter())
        .take_while(|(a, b)| equivalent_component(**a, **b))
        .count();
    let mut result = PathBuf::new();
    if common == from.len() {
        // Same directory: an explicit `./` marks this as a *path*, not a
        // bare filename — otherwise a wiki resolver, seeing no separator,
        // would send an accepted candidate back through a global basename
        // lookup that a same-named file elsewhere could make ambiguous,
        // even though a specific file was just chosen (Codex preliminary
        // review, engine corrections).
        result.push(".");
    }
    for _ in common..from.len() {
        result.push("..");
    }
    for component in &target[common..] {
        result.push(component.as_os_str());
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::FileStamp;

    fn entry(root: &str, relative: &str, headings: Vec<document::Heading>) -> Entry {
        let root = PathBuf::from(root);
        let relative = PathBuf::from(relative);
        let canonical = root.join(&relative);
        Entry {
            root,
            relative,
            canonical,
            fingerprint: FileStamp {
                modified: None,
                length: 0,
            },
            headings,
            headings_complete: true,
        }
    }

    fn heading(level: u8, text: &str) -> document::Heading {
        document::Heading {
            level,
            text: text.to_owned(),
            at: 0,
        }
    }

    #[test]
    fn wiki_file_trigger_is_found_right_after_double_brackets() {
        let source = "本文 [[第一";
        let context = detect(source, source.len()).expect("finds a trigger");
        assert_eq!(context.kind, TriggerKind::WikiFile);
        assert_eq!(context.query, "第一");
        assert_eq!(&source[context.trigger_at..context.trigger_at + 2], "[[");
    }

    #[test]
    fn wiki_heading_trigger_carries_the_file_typed_before_the_hash() {
        let source = "[[次の原稿.md#第";
        let context = detect(source, source.len()).expect("finds a trigger");
        assert_eq!(
            context.kind,
            TriggerKind::WikiHeading {
                file: "次の原稿.md".to_owned()
            }
        );
        assert_eq!(context.query, "第");
    }

    #[test]
    fn a_same_document_wiki_heading_trigger_has_an_empty_file() {
        let source = "[[#見出";
        let context = detect(source, source.len()).expect("finds a trigger");
        assert_eq!(
            context.kind,
            TriggerKind::WikiHeading {
                file: String::new()
            }
        );
    }

    #[test]
    fn markdown_file_trigger_is_found_after_the_closing_bracket_and_paren() {
        let source = "[見よ](次の";
        let context = detect(source, source.len()).expect("finds a trigger");
        assert_eq!(context.kind, TriggerKind::MarkdownFile);
        assert_eq!(context.query, "次の");
    }

    #[test]
    fn markdown_heading_trigger_carries_the_file_typed_before_the_hash() {
        let source = "[見よ](次の.md#第";
        let context = detect(source, source.len()).expect("finds a trigger");
        assert_eq!(
            context.kind,
            TriggerKind::MarkdownHeading {
                file: "次の.md".to_owned()
            }
        );
        assert_eq!(context.query, "第");
    }

    #[test]
    fn a_same_document_markdown_heading_trigger_has_an_empty_file() {
        let source = "[見よ](#見出";
        let context = detect(source, source.len()).expect("finds a trigger");
        assert_eq!(
            context.kind,
            TriggerKind::MarkdownHeading {
                file: String::new()
            }
        );
    }

    #[test]
    fn a_closed_link_is_not_an_active_trigger() {
        let wiki = "[[閉じた]] その後";
        assert!(detect(wiki, wiki.len()).is_none());
        let markdown = "[見よ](閉じた) その後";
        assert!(detect(markdown, markdown.len()).is_none());
    }

    #[test]
    fn detection_never_crosses_a_line_break() {
        let source = "[[前の行\n次の行";
        assert!(detect(source, source.len()).is_none());
    }

    #[test]
    fn an_out_of_bounds_or_mid_character_caret_is_refused() {
        let source = "日本語";
        assert!(detect(source, source.len() + 1).is_none());
        assert!(detect(source, 1).is_none());
    }

    #[test]
    fn replacement_ranges_are_validated_for_utf8() {
        let source = "日本語";
        assert!(valid_replacement_range(source, 0..source.len()));
        assert!(!valid_replacement_range(source, 0..1));
        assert!(!valid_replacement_range(source, 0..(source.len() + 1)));
        // 頭と尻が逆の範囲も断る（わざと空の範囲を作る）。
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = 3..0;
        assert!(!valid_replacement_range(source, reversed));
    }

    #[test]
    fn already_closed_is_detected_from_what_follows_the_caret() {
        // Caret right after "部分", right before the closing "]]".
        let wiki = "[[部分]]";
        let caret = "[[部分".len();
        let context = detect(wiki, caret).expect("finds a trigger inside the brackets");
        assert!(context.already_closed);

        let unclosed = "[[部分";
        let context = detect(unclosed, unclosed.len()).expect("finds a trigger");
        assert!(!context.already_closed);
    }

    /// 書き手の報告 2026-09-21: the caret can sit *inside* a target that is
    /// already written. The replacement has to end where the target ends, not
    /// at the caret — ending at the caret left the tail behind and wrote a
    /// second closer (`[[Target#Target%20heading%20Test]]ing]]`).
    #[test]
    fn a_written_target_ends_the_replacement_where_it_ends() {
        let source = "[[Target#Target heading]]";
        let caret = source.find("ing").expect("typing inside the heading");
        let context = detect(source, caret).expect("finds the heading trigger");
        assert_eq!(context.query, "Target head");
        assert_eq!(context.target_end, Some(source.len() - 2));
        assert!(context.already_closed);

        // An alias bar ends the target too, so the alias survives.
        let source = "[[Target#Target heading|表示名]]";
        let caret = source.find("ing").expect("typing inside the heading");
        let context = detect(source, caret).expect("finds the heading trigger");
        assert_eq!(context.target_end, source.find('|'));
        assert!(context.already_closed);

        // Nothing ends it yet: the caret is the end, and a closer is added.
        let source = "[[Target#Target head";
        let context = detect(source, source.len()).expect("finds the heading trigger");
        assert_eq!(context.target_end, None);
        assert!(!context.already_closed);
    }

    #[test]
    fn candidate_limit_is_clamped_and_zero_yields_nothing() {
        let entries: Vec<Entry> = (0..5)
            .map(|n| entry("/根", &format!("f{n}.md"), Vec::new()))
            .collect();
        let context = detect("[[", 2).unwrap();

        assert!(candidates(&entries, None, "", &context, 0).is_empty());
        assert_eq!(
            candidates(&entries, None, "", &context, usize::MAX).len(),
            5
        );

        let many: Vec<Entry> = (0..150)
            .map(|n| entry("/根", &format!("f{n}.md"), Vec::new()))
            .collect();
        assert_eq!(
            candidates(&many, None, "", &context, usize::MAX).len(),
            DEFAULT_CANDIDATE_LIMIT
        );
    }

    #[test]
    fn file_candidates_are_filtered_case_insensitively() {
        let entries = vec![
            entry("/一", "Memo.md", Vec::new()),
            entry("/二", "MEMO.md", Vec::new()),
            entry("/一", "別件.md", Vec::new()),
        ];
        let context = detect("[[memo", 6).unwrap();

        let found = candidates(&entries, None, "", &context, DEFAULT_CANDIDATE_LIMIT);
        assert_eq!(found.len(), 2);
    }

    /// RFN01-48: **画像の記法かどうか。**`!`が記法の頭に付いているものだけが画像で、
    /// `[[`と`[見よ](`は今までどおり普通のリンクである。
    #[test]
    fn the_trigger_says_whether_it_is_an_image() {
        for source in ["![[絵", "![説明](絵"] {
            let context = detect(source, source.len()).expect("finds a trigger");
            assert!(context.image, "{source}");
        }
        for source in ["[[絵", "[見よ](絵"] {
            let context = detect(source, source.len()).expect("finds a trigger");
            assert!(!context.image, "{source}");
        }
    }

    /// RFN01-48: **画像の記法では画像だけを候補にする。**普通のリンクは今までどおり
    /// ——画像も候補に出る（`[[絵.png]]`は開く先として使える）。
    #[test]
    fn an_image_trigger_offers_images_only() {
        let entries = vec![
            entry("/根", "原稿.md", Vec::new()),
            entry("/根", "絵.png", Vec::new()),
            entry("/根", "挿絵.jpg", Vec::new()),
        ];

        let wiki = detect("![[絵", "![[絵".len()).unwrap();
        let mut names: Vec<String> = candidates(&entries, None, "", &wiki, DEFAULT_CANDIDATE_LIMIT)
            .iter()
            .map(|c| c.display.clone())
            .collect();
        names.sort();
        let mut wanted = vec!["根 / 絵.png".to_owned(), "根 / 挿絵.jpg".to_owned()];
        wanted.sort();
        assert_eq!(names, wanted);

        let markdown = detect("![説明](絵", "![説明](絵".len()).unwrap();
        assert_eq!(
            candidates(&entries, None, "", &markdown, DEFAULT_CANDIDATE_LIMIT).len(),
            2
        );

        // 普通のリンクは、画像も候補のまま（開く先として使える）。ここは絞り込みの
        // 話なので、問いを空にして3つとも出ることを見る。
        let plain = detect("[[", "[[".len()).unwrap();
        assert_eq!(
            candidates(&entries, None, "", &plain, DEFAULT_CANDIDATE_LIMIT).len(),
            3
        );
    }

    /// RFN01-48: **画像に見出しは無い。**`![[絵.png#`では候補を出さない。
    #[test]
    fn an_image_trigger_offers_no_headings() {
        let entries = vec![
            entry("/根", "絵.png", Vec::new()),
            entry("/根", "原稿.md", vec![heading(1, "第一章")]),
        ];
        let source = "![[絵.png#";
        let context = detect(source, source.len()).unwrap();

        assert!(context.image);
        assert!(candidates(&entries, None, "", &context, DEFAULT_CANDIDATE_LIMIT).is_empty());
    }

    #[test]
    fn same_named_files_get_full_root_qualified_displays_only_when_they_collide() {
        // Two different roots that both happen to be named "根", each
        // holding a file also named the same — a basename-only root label
        // would make these indistinguishable.
        let entries = vec![
            entry("/一/根", "同じ.md", Vec::new()),
            entry("/二/根", "同じ.md", Vec::new()),
            entry("/三/よそ", "違う.md", Vec::new()),
        ];
        let context = detect("[[", 2).unwrap();

        let found = candidates(&entries, None, "", &context, DEFAULT_CANDIDATE_LIMIT);
        let mut displays: Vec<&str> = found.iter().map(|c| c.display.as_str()).collect();
        displays.sort();
        assert_eq!(
            displays,
            vec!["/一/根 / 同じ.md", "/二/根 / 同じ.md", "よそ / 違う.md"]
        );
    }

    #[test]
    fn a_wiki_insertion_uses_the_path_relative_to_the_source_files_folder() {
        let entries = vec![entry("/根", "章/二.md", Vec::new())];
        let source_file = Path::new("/根/章/一.md");
        let context = detect("[[二", 5).unwrap();

        let found = candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        );
        assert_eq!(found.len(), 1);
        // A leading `./` marks this as a same-directory *path* rather than a
        // bare filename, so a resolver never sends it back through a global
        // basename lookup that another root's same-named file could make
        // ambiguous (Codex preliminary review, engine corrections).
        assert_eq!(found[0].insert, "./二.md]]");
        assert_eq!(found[0].caret_after_insert, "./二.md".len());
    }

    #[test]
    fn a_wiki_insertion_is_the_full_path_when_there_is_no_source_file() {
        let entries = vec![entry("/よそ", "先.md", Vec::new())];
        let context = detect("[[先", 5).unwrap();

        let found = candidates(&entries, None, "", &context, DEFAULT_CANDIDATE_LIMIT);
        assert_eq!(found[0].insert, "/よそ/先.md]]");
    }

    #[test]
    fn accepting_a_candidate_inside_an_already_closed_link_does_not_duplicate_the_closer() {
        let entries = vec![entry("/根", "次.md", Vec::new())];
        let source_file = Path::new("/根/現在.md");
        let source = "[[]]";
        // Caret lands right after `[[`, with `]]` already sitting past it.
        let context = detect(source, 2).unwrap();
        assert!(context.already_closed);

        let found = candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        );
        assert_eq!(found[0].insert, "./次.md");
        assert_eq!(found[0].caret_after_insert, found[0].insert.len());
    }

    #[test]
    fn reserved_bytes_are_percent_encoded_and_japanese_is_untouched() {
        let entries = vec![entry("/根", "第 一章|草稿#note.md", Vec::new())];
        let source_file = Path::new("/根/現在.md");
        let context = detect("[[", 2).unwrap();

        let found = candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        );
        // `|`, `#`, and the space are encoded; every Japanese character and
        // the ASCII letters/digits pass through untouched; `./` marks this
        // same-directory file as a path (see `relative_between`).
        assert_eq!(found[0].insert, "./第%20一章%7C草稿%23note.md]]");
    }

    #[test]
    fn percent_encode_and_decode_round_trip() {
        let original = "第 一章|草稿#note%done.md";
        let encoded = percent_encode_reserved(original, &WIKI_RESERVED);
        assert_eq!(percent_decode(&encoded), original);
    }

    #[test]
    fn split_target_heading_splits_at_the_first_literal_hash() {
        assert_eq!(
            split_target_heading("次.md%23with-percent#見出し"),
            ("次.md%23with-percent", Some("見出し"))
        );
        assert_eq!(split_target_heading("次.md"), ("次.md", None));
    }

    #[test]
    fn a_generated_wiki_link_parses_with_the_expected_target_boundaries() {
        let entries = vec![entry("/根", "第 一章.md", Vec::new())];
        let source_file = Path::new("/根/現在.md");
        let typed = "[[";
        let context = detect(typed, typed.len()).unwrap();
        let candidate = &candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        )[0];

        let full_line = format!("{typed}{}", candidate.insert);
        let (target, is_wiki) = document::link_target_at(&full_line, 3).expect("parses as a link");
        assert!(is_wiki);
        assert_eq!(percent_decode(target), "./第 一章.md");
    }

    #[test]
    fn a_generated_markdown_link_parses_with_the_expected_target_boundaries() {
        let entries = vec![entry("/根", "第(一)章.md", Vec::new())];
        let source_file = Path::new("/根/現在.md");
        let typed = "[見よ](";
        let context = detect(typed, typed.len()).unwrap();
        let candidate = &candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        )[0];

        let full_line = format!("{typed}{}", candidate.insert);
        let byte = typed.len();
        let (target, is_wiki) =
            document::link_target_at(&full_line, byte).expect("parses as a link");
        assert!(!is_wiki);
        assert_eq!(percent_decode(target), "./第(一)章.md");
    }

    /// RFN01-43: **拡張子を省いた名前でも、見出しの候補が出ること。**
    /// 書き手の確認（2026-09-21）：同じフォルダなら出る，違うフォルダだと出ない。
    #[test]
    fn a_heading_candidate_is_found_without_the_extension() {
        let found = |folder: &str| {
            let entries = vec![entry(folder, "次.md", vec![heading(1, "第一章")])];
            let source = "[[次#第一";
            let context = detect(source, source.len()).unwrap();
            let source_file = Path::new("/根/現在.md");
            candidates(
                &entries,
                Some(source_file),
                "",
                &context,
                DEFAULT_CANDIDATE_LIMIT,
            )
            .len()
        };

        assert_eq!(found("/根"), 1, "同じフォルダ");
        assert_eq!(found("/根/別"), 1, "違うフォルダ（RFN01-43）");
    }

    #[test]
    fn heading_candidates_come_from_the_named_files_own_headings() {
        let entries = vec![entry(
            "/根",
            "次.md",
            vec![heading(1, "第一章"), heading(2, "第二節")],
        )];
        let source = "[[次.md#第一";
        let context = detect(source, source.len()).unwrap();
        let source_file = Path::new("/根/現在.md");

        let found = candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading.as_deref(), Some("第一章"));
        assert_eq!(found[0].heading_at, Some(0));
        assert_eq!(found[0].insert, "第一章]]");
    }

    #[test]
    fn markdown_heading_candidates_come_from_the_named_files_own_headings() {
        let entries = vec![entry("/根", "次.md", vec![heading(1, "第一章")])];
        let source = "[見よ](次.md#第";
        let context = detect(source, source.len()).unwrap();
        let source_file = Path::new("/根/現在.md");

        let found = candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].insert, "第一章)");
    }

    #[test]
    fn duplicate_heading_labels_are_distinguished_by_occurrence_and_position() {
        let entries = vec![entry(
            "/根",
            "次.md",
            vec![
                document::Heading {
                    level: 1,
                    text: "まとめ".to_owned(),
                    at: 0,
                },
                document::Heading {
                    level: 2,
                    text: "詳細".to_owned(),
                    at: 10,
                },
                document::Heading {
                    level: 1,
                    text: "まとめ".to_owned(),
                    at: 40,
                },
            ],
        )];
        let source = "[[次.md#";
        let context = detect(source, source.len()).unwrap();
        let source_file = Path::new("/根/現在.md");

        let found = candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        );
        let summaries: Vec<(String, Option<usize>)> = found
            .iter()
            .map(|c| (c.display.clone(), c.heading_at))
            .collect();
        assert_eq!(
            summaries,
            vec![
                ("まとめ (1/2)".to_owned(), Some(0)),
                ("詳細".to_owned(), Some(10)),
                ("まとめ (2/2)".to_owned(), Some(40)),
            ]
        );
        // A duplicate heading's `insert` carries its own occurrence, so
        // accepting the *second* "まとめ" does not silently point back at the
        // first when the link is later resolved (Additional reviewed
        // integration decisions, 2026-09-18).
        let inserts: Vec<&str> = found.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(inserts, vec!["まとめ#1]]", "詳細]]", "まとめ#2]]"]);
    }

    /// Additional reviewed integration decisions, 2026-09-18: comparing a
    /// decoded query against a still-encoded candidate path meant a file
    /// with a space (or any other reserved byte) in its name never matched
    /// its own heading list.
    #[test]
    fn heading_candidates_match_a_typed_file_that_needed_percent_encoding() {
        let entries = vec![entry("/根", "第 一章.md", vec![heading(1, "第一節")])];
        let typed_file = percent_encode_reserved("第 一章.md", &WIKI_RESERVED);
        let source = format!("[[{typed_file}#");
        let context = detect(&source, source.len()).unwrap();
        let source_file = Path::new("/根/現在.md");

        let found = candidates(
            &entries,
            Some(source_file),
            "",
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading.as_deref(), Some("第一節"));
    }

    #[test]
    fn a_same_document_heading_candidate_reads_the_live_unsaved_text_not_the_index() {
        let stale_index = vec![entry("/根", "本文.md", vec![heading(1, "古い見出し")])];
        let live_source = "# 新しい見出し\n本文";
        let trigger_source = "[[#新";
        let context = detect(trigger_source, trigger_source.len()).unwrap();

        let found = candidates(
            &stale_index,
            None,
            live_source,
            &context,
            DEFAULT_CANDIDATE_LIMIT,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading.as_deref(), Some("新しい見出し"));
    }

    #[test]
    fn an_unknown_file_before_the_hash_yields_no_heading_candidates() {
        let entries = vec![entry("/根", "次.md", vec![heading(1, "第一章")])];
        let source = "[[いない.md#";
        let context = detect(source, source.len()).unwrap();

        let found = candidates(&entries, None, "", &context, DEFAULT_CANDIDATE_LIMIT);
        assert!(found.is_empty());
    }

    #[test]
    fn detecting_and_building_candidates_never_mutates_the_source() {
        let source = "本文 [[検".to_owned();
        let before = source.clone();
        let entries = vec![entry("/根", "検索対象.md", Vec::new())];

        let context = detect(&source, source.len()).expect("finds a trigger");
        let _ = candidates(&entries, None, &source, &context, DEFAULT_CANDIDATE_LIMIT);

        assert_eq!(source, before);
    }

    /// Additional reviewed integration decisions, 2026-09-18: `canonicalize`
    /// hands back `\\?\C:\...`, whose naive `\`-to-`/` replacement
    /// (`//?/C:/...`) is not a path anything can reopen.
    #[cfg(windows)]
    #[test]
    fn a_windows_verbatim_disk_path_is_normalized_without_the_prefix() {
        let entries = vec![entry(r"\\?\C:\根", "次.md", Vec::new())];
        let context = detect("[[", 2).unwrap();

        let found = candidates(&entries, None, "", &context, DEFAULT_CANDIDATE_LIMIT);
        assert_eq!(found[0].insert, "C:/根/次.md]]");
    }

    #[cfg(windows)]
    #[test]
    fn normal_source_and_verbatim_target_use_relative_paths_on_the_same_drive() {
        let context = detect("[[", 2).unwrap();
        for (source, root, expected) in [
            (r"D:\root\current.md", r"\\?\D:\root", "./next.md]]"),
            (r"d:\root\current.md", r"\\?\D:\root", "./next.md]]"),
            (
                r"D:\root\one\current.md",
                r"\\?\D:\root\two",
                "../two/next.md]]",
            ),
            (r"D:\root\current.md", r"\\?\E:\root", "E:/root/next.md]]"),
            (
                r"\\server\share\root\current.md",
                r"\\?\UNC\server\share\root",
                "./next.md]]",
            ),
            (r"D:\ROOT\current.md", r"\\?\D:\root", "../root/next.md]]"),
        ] {
            let entries = vec![entry(root, "next.md", Vec::new())];
            let found = candidates(&entries, Some(Path::new(source)), "", &context, 10);
            assert_eq!(found[0].insert, expected, "source={source}, root={root}");
        }
    }

    /// A verbatim UNC path (`\\?\UNC\server\share\...`) normalizes to the
    /// same `//server/share/...` an ordinary UNC path already did — the
    /// prefix is what gets stripped, not the server/share themselves.
    #[cfg(windows)]
    #[test]
    fn a_windows_verbatim_unc_path_keeps_its_server_and_share() {
        let entries = vec![entry(r"\\?\UNC\server\share", "次.md", Vec::new())];
        let context = detect("[[", 2).unwrap();

        let found = candidates(&entries, None, "", &context, DEFAULT_CANDIDATE_LIMIT);
        assert_eq!(found[0].insert, "//server/share/次.md]]");
    }
}
