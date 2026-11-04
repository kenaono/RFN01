//! RFN01-88: templates — Markdown files a new file starts from, or that are put
//! in at the caret.
//!
//! **書き手の決定（2026-11-03〜04）**:
//! - A Workspace names one folder as its Templates Folder (any folder in it).
//! - The templates are the `.md` files directly in that folder; the folders
//!   below it are not looked into. A template is called by its file name
//!   without the `.md`.
//! - A file made from a template is a `.md` file. A name typed without an
//!   extension is given `.md`; a name with another extension is refused.
//! - `{{title}}`, `{{date}}` and `{{time}}` are replaced, nothing else.

use std::path::{Path, PathBuf};

use crate::timestamp::{self, LocalTime};

/// One template: what the writer sees and the file it is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    pub name: String,
    pub path: PathBuf,
}

/// The templates in `folder`, by name. Nothing for a folder that cannot be read.
pub fn list(folder: &Path) -> Vec<Template> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<Template> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let path = entry.path();
            let name = markdown_stem(&path)?;
            Some(Template { name, path })
        })
        .collect();
    found.sort_by_cached_key(|template| template.name.to_lowercase());
    found
}

/// The name without `.md`, for a path that ends in `.md` (in any case).
fn markdown_stem(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?;
    if !extension.eq_ignore_ascii_case("md") {
        return None;
    }
    Some(path.file_stem()?.to_string_lossy().into_owned())
}

/// The name a file made from a template is given, or `None` when the writer
/// typed another extension than `.md`.
pub fn file_name(typed: &str) -> Option<String> {
    match Path::new(typed)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        None => Some(format!("{typed}.md")),
        Some(extension) if extension.eq_ignore_ascii_case("md") => Some(typed.to_owned()),
        Some(_) => None,
    }
}

/// What a template says, with its three words replaced. `title` is the file's
/// name without its extension, empty for an untitled tab.
pub fn fill(text: &str, title: &str, now: LocalTime) -> String {
    text.replace("{{title}}", title)
        .replace("{{date}}", &timestamp::format("yyyy-MM-dd", now))
        .replace("{{time}}", &timestamp::format("HH:mm", now))
}

/// A template's text as the editor holds text: no byte-order mark, and `\n`
/// for every line break. `None` for a file that is not UTF-8.
pub fn read(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Some(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// The title `{{title}}` stands for: the file's name without its extension.
pub fn title_of(path: Option<&Path>) -> String {
    path.and_then(Path::file_stem)
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u16, minute: u16) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 11,
            day: 4,
            hour,
            minute,
            ..LocalTime::default()
        }
    }

    #[test]
    fn the_three_words_are_replaced_and_nothing_else() {
        let text = "# {{title}}\n{{date}} {{time}}\n{{Title}} {{other}} {{title}}";
        assert_eq!(
            fill(text, "第1章", at(9, 5)),
            "# 第1章\n2026-11-04 09:05\n{{Title}} {{other}} 第1章"
        );
        assert_eq!(fill("{{title}}", "", at(14, 30)), "");
        assert_eq!(fill("{{time}}", "", at(14, 30)), "14:30");
    }

    #[test]
    fn a_file_from_a_template_is_markdown() {
        assert_eq!(file_name("メモ").as_deref(), Some("メモ.md"));
        assert_eq!(file_name("メモ.md").as_deref(), Some("メモ.md"));
        assert_eq!(file_name("メモ.MD").as_deref(), Some("メモ.MD"));
        assert_eq!(file_name("メモ.txt"), None);
        assert_eq!(file_name("v1.2"), None);
    }

    #[test]
    fn only_markdown_files_directly_in_the_folder_are_templates() {
        let root = std::env::temp_dir().join(format!("rfn-templates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("下のフォルダ")).unwrap();
        std::fs::write(root.join("日記.md"), "a").unwrap();
        std::fs::write(root.join("章.MD"), "b").unwrap();
        std::fs::write(root.join("メモ.txt"), "c").unwrap();
        std::fs::write(root.join("下のフォルダ").join("奥.md"), "d").unwrap();
        std::fs::create_dir_all(root.join("フォルダ.md")).unwrap();
        let names: Vec<String> = list(&root).into_iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"日記".to_owned()));
        assert!(names.contains(&"章".to_owned()));
        assert!(list(&root.join("無い")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_template_is_read_as_the_editor_holds_text() {
        let path = std::env::temp_dir().join(format!("rfn-template-{}.md", std::process::id()));
        std::fs::write(&path, "\u{feff}一\r\n二\r三\n").unwrap();
        assert_eq!(read(&path).as_deref(), Some("一\n二\n三\n"));
        std::fs::write(&path, [0xffu8, 0xfe, 0x00]).unwrap();
        assert_eq!(read(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_title_is_the_name_without_its_extension() {
        assert_eq!(title_of(Some(Path::new(r"D:\原稿\第1章.md"))), "第1章");
        assert_eq!(title_of(None), "");
    }
}
