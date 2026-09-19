//! Pure rename/move planning. The caller owns source eligibility and persistence.
use std::path::{Component, Path, PathBuf};

use crate::{
    document, file_tree, link_completion,
    workspace_index::Entry,
    workspace_links::{self, ResolvedLink},
};

/// Update resolvable local targets against the pre-move index. Never reads or writes disk.
/// Labels, fragment spelling, code, images and unresolved/ambiguous links stay untouched.
pub fn rewrite(
    source_text: &str,
    source_before: &Path,
    source_after: &Path,
    from: &Path,
    to: &Path,
    entries: &[Entry],
) -> String {
    let before = normalized(source_before);
    let after = normalized(source_after);
    let from = normalized(from);
    let to = normalized(to);
    let mut output = source_text.to_owned();
    for (range, wiki) in document::link_target_ranges(source_text).into_iter().rev() {
        let raw = &source_text[range.clone()];
        let trimmed = raw.trim();
        let wrapped = trimmed.starts_with('<') && trimmed.ends_with('>');
        let target = if wrapped {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        };
        let (file, _) = link_completion::split_target_heading(target);
        let decoded_file = link_completion::percent_decode(file);
        if decoded_file.contains(':') && !Path::new(&decoded_file).is_absolute() {
            continue;
        }
        let Ok(ResolvedLink::Target { path, .. }) =
            workspace_links::resolve_link(target, wiki, Some(&before), source_text, entries)
        else {
            continue;
        };
        let path = normalized(&path);
        let moved = file_tree::moved_path(&from, &to, &path).unwrap_or_else(|| path.clone());
        // A moved source and target can keep the same relative spelling, as can bare names.
        if let Ok(ResolvedLink::Target { path: still, .. }) =
            workspace_links::resolve_link(target, wiki, Some(&after), source_text, entries)
        {
            if normalized(&still) == moved {
                continue;
            }
        }
        if moved == path && before.parent() == after.parent() {
            continue;
        }
        let fragment = &target[file.len()..];
        let absolute = Path::new(&decoded_file).is_absolute();
        let path_text = link_completion::target_path_text(
            &moved,
            if absolute { None } else { Some(&after) },
            wiki,
        );
        let target = if wrapped {
            format!("<{path_text}{fragment}>")
        } else {
            format!("{path_text}{fragment}")
        };
        let leading = raw.len() - raw.trim_start().len();
        let trailing = raw.trim_end().len();
        output.replace_range(
            range,
            &format!("{}{target}{}", &raw[..leading], &raw[trailing..]),
        );
    }
    output
}

fn normalized(path: &Path) -> PathBuf {
    let mut spelling = link_completion::path_to_string(path);
    #[cfg(windows)]
    if spelling.as_bytes().get(1) == Some(&b':') {
        let drive = spelling[..1].to_ascii_uppercase();
        spelling.replace_range(..1, &drive);
    }
    let mut out = PathBuf::new();
    for component in Path::new(&spelling).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if out.file_name().is_some_and(|name| name != "..") => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::FileStamp;
    fn path(name: &str) -> PathBuf {
        Path::new(if cfg!(windows) { "C:/root" } else { "/root" }).join(name)
    }
    fn entry(name: &str) -> Entry {
        Entry {
            root: path(""),
            relative: name.into(),
            canonical: path(name),
            fingerprint: FileStamp {
                modified: None,
                length: 0,
            },
            headings: vec![],
            headings_complete: true,
        }
    }
    #[test]
    fn target_move_preserves_aliases_and_raw_fragments() {
        let source = "[[./old.md#見出し#2|表示名]] [説明](./old.md#first)";
        assert_eq!(
            rewrite(
                source,
                &path("source.md"),
                &path("source.md"),
                &path("old.md"),
                &path("new.md"),
                &[]
            ),
            "[[./new.md#見出し#2|表示名]] [説明](./new.md#first)"
        );
    }
    #[test]
    fn folder_move_keeps_internal_relative_links_and_updates_external_sources() {
        let source = "[next](next.md) [[../outside.md]]";
        assert_eq!(
            rewrite(
                source,
                &path("a/current.md"),
                &path("b/current.md"),
                &path("a"),
                &path("b"),
                &[]
            ),
            source
        );
        assert_eq!(
            rewrite(
                "[[./a/next.md]]",
                &path("current.md"),
                &path("current.md"),
                &path("a"),
                &path("b"),
                &[]
            ),
            "[[./b/next.md]]"
        );
    }
    #[test]
    fn moving_source_rebases_links_to_stationary_targets() {
        assert_eq!(
            rewrite(
                "[next](../next.md)",
                &path("a/source.md"),
                &path("a/deeper/source.md"),
                &path("a/source.md"),
                &path("a/deeper/source.md"),
                &[]
            ),
            "[next](../../next.md)"
        );
    }
    #[test]
    fn encoded_paths_round_trip_without_decoding_fragment_or_percent_twice() {
        let source = "[[./old.md#第%23一章|名前]]";
        assert_eq!(
            rewrite(
                source,
                &path("source.md"),
                &path("source.md"),
                &path("old.md"),
                &path("新 # %23.md"),
                &[]
            ),
            "[[./新%20%23%20%2523.md#第%23一章|名前]]"
        );
    }
    #[test]
    fn excluded_and_ambiguous_links_remain_unchanged() {
        let source = "`[[./old.md]]` ![image](./old.md) \\[[./old.md]]\n```\n[[./old.md]]\n```\n[[old.md]] [web](https://example.com/old.md) [[unknown.md]]";
        assert_eq!(
            rewrite(
                source,
                &path("source.md"),
                &path("source.md"),
                &path("old.md"),
                &path("new.md"),
                &[entry("old.md"), entry("other/old.md")]
            ),
            source
        );
    }
    #[test]
    fn unique_bare_wiki_target_becomes_qualified_after_move() {
        assert_eq!(
            rewrite(
                "[[old.md|alias]]",
                &path("source.md"),
                &path("source.md"),
                &path("old.md"),
                &path("sub/new.md"),
                &[entry("old.md")]
            ),
            "[[./sub/new.md|alias]]"
        );
    }
    #[test]
    fn moving_source_does_not_rewrite_external_schemes() {
        let source = "[mail](mailto:writer@example.com) [web](https://example.com)";
        assert_eq!(
            rewrite(
                source,
                &path("source.md"),
                &path("sub/source.md"),
                &path("source.md"),
                &path("sub/source.md"),
                &[]
            ),
            source
        );
    }

    #[test]
    fn case_only_and_sequential_folder_moves_keep_the_latest_target() {
        let source = path("source.md");
        let first = rewrite(
            "[[./Readme.md|label]]",
            &source,
            &source,
            &path("Readme.md"),
            &path("readme.md"),
            &[],
        );
        assert_eq!(first, "[[./readme.md|label]]");
        let second = rewrite(
            &first,
            &source,
            &source,
            &path("readme.md"),
            &path("folder/readme.md"),
            &[],
        );
        assert_eq!(second, "[[./folder/readme.md|label]]");
        let third = rewrite(
            &second,
            &source,
            &source,
            &path("folder"),
            &path("renamed"),
            &[],
        );
        assert_eq!(third, "[[./renamed/readme.md|label]]");
    }
}
