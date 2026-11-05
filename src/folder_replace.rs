//! RFN01-70: 左Paneの検索から、フォルダのファイルをまとめて置換する。
//!
//! **探し方は本文の検索と同じ**（`find::Search`）、**書き換えるファイルは検索と同じ並び**
//! （`searcher::searched_files`）。数えてから訊き、答えが来たら読み直して置き換える。
//!
//! * 開いている文書は、保存していてもいなくても**TABの本文を置き換える**。未保存のまま
//!   残り、1回のUndoで戻る（書き手の判断 2026-10-24）。
//! * 開いていないファイルは、**バックアップを取ってから**書き換える。文字コードと改行は
//!   元のまま。バックアップが取れなければ書き換えない。
use crate::*;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
};

/// 訊いているあいだ持っておく、置換の頼み。答えが来たら、この語と探し方でやり直す。
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    needle: String,
    replacement: String,
    rules: find::Rules,
    files: Vec<PathBuf>,
}

/// 「すべて置換」が押された。数えて、確かめる。
pub(crate) fn requested(window: &AppWindow, live: &Live) {
    let needle = window.get_folder_needle().to_string();
    if needle.is_empty() {
        return;
    }
    if !tags::split_query(&needle).0.is_empty() {
        let told = pick(
            "tag: の検索では置換できません",
            "Cannot replace in a tag: search",
        );
        window.set_folder_status(told.into());
        return;
    }
    let rules = folder_rules(window);
    let search = match find::Search::new(&needle, rules) {
        Ok(search) => search,
        Err(trouble) => {
            window.set_folder_status(trouble.into());
            return;
        }
    };
    let roots = folder_search_roots(live, &needle);
    if roots.is_empty() {
        window.set_folder_status(no_work_folder().into());
        return;
    }
    let exclusions = window.get_search_exclusions().to_string();
    let walked = searcher::searched_files(&roots, &exclusions, SEARCHED_FILES, &mut || false);
    let documents = open_documents(live);
    let (mut files, mut places, mut open) = (Vec::new(), 0usize, 0usize);
    for path in walked.unwrap_or_default() {
        let held = held_document(&documents, &path);
        let count = match &held {
            Some(document) => search.tally(&document.text.borrow(), None).0,
            None => match file_io::read(&path, MAX_DOCUMENT_CHARACTERS) {
                Ok(loaded) => search.tally(&loaded.text, None).0,
                Err(_) => 0,
            },
        };
        if count == 0 {
            continue;
        }
        places += count;
        open += usize::from(held.is_some());
        files.push(path);
    }
    if files.is_empty() {
        let told = say!("「{needle}」は見つかりません", "\"{needle}\" not found");
        window.set_folder_status(told.into());
        return;
    }
    let replacement = window.get_folder_replacement().to_string();
    let count = files.len();
    let text = say!(
        "「{needle}」を「{replacement}」に置換します。\n\n\
         {count}ファイル・{places}箇所（開いているTAB {open}ファイルを含む）。\n\
         開いているTABは本文を置き換え、保存せずに残します。\n\
         開いていないファイルは、バックアップを取ってから書き換えます。",
        "Replace \"{needle}\" with \"{replacement}\".\n\n\
         {count} files, {places} places ({open} files open in tabs).\n\
         Open tabs are changed and left unsaved.\n\
         Files that are not open are backed up, then rewritten."
    );
    let plan = Plan {
        needle,
        replacement,
        rules,
        files,
    };
    ask_question(
        window,
        live,
        Question::FolderReplace(plan),
        text,
        &[pick("置換", "Replace"), cancel()],
        0,
    );
}

/// 「置換」と答えた。**数えたときから変わっていても、いまの中身で置き換える。**
pub(crate) fn confirmed(window: &AppWindow, live: &Live, plan: Plan) {
    let Ok(search) = find::Search::new(&plan.needle, plan.rules) else {
        return;
    };
    let store = saving::backup_store(window);
    let keep = window.get_backup_keep().clamp(1, backup::MAX_KEEP as i32) as usize;
    let (mut files, mut places, mut refused, mut changed) = (0usize, 0usize, 0usize, 0usize);
    for path in &plan.files {
        let documents = open_documents(live);
        let replaced = match held_document(&documents, path) {
            Some(document) => replace_open(window, live, &document, &search, &plan.replacement)
                .map_err(|()| Refused::Other),
            None => replace_closed(live, path, &search, &plan.replacement, store.as_ref(), keep),
        };
        match replaced {
            Ok(0) => {}
            Ok(count) => {
                files += 1;
                places += count;
            }
            Err(Refused::Changed) => changed += 1,
            Err(Refused::Other) => refused += 1,
        }
    }
    let told = if refused == 0 {
        say!(
            "{files}ファイル・{places}箇所を置換しました",
            "Replaced {places} places in {files} files"
        )
    } else {
        say!(
            "{files}ファイル・{places}箇所を置換しました（{refused}ファイルは書き換えられませんでした）",
            "Replaced {places} places in {files} files ({refused} files could not be changed)"
        )
    };
    let told = if changed == 0 {
        told
    } else {
        say!(
            "{told}。外部で変更された {changed} ファイルは置換しませんでした",
            "{told}. Skipped {changed} files changed outside RFN Edit"
        )
    };
    live.cache.borrow_mut().log_diag(
        "search",
        &format!("replace files={files} places={places} refused={refused} changed={changed}"),
    );
    publish_tabs(window, live);
    crate::backup_ui::refresh_pane(window, live);
    window.tell(told.clone().into());
    // 探し直す。いま残っている一致（書き換えられなかったもの）が一覧に出る。
    live.folder.borrow_mut().replaced_note = Some(told);
    search_work_folder(window, live);
}

/// このファイルを開いている文書（同じファイルは1つの文書を共有する）。
///
/// 一覧と同じ見分け方（`file_tree::path_key`）——一覧がTABの本文で数えたファイルを、
/// ここでもTABの本文で置き換える。
fn held_document(documents: &[Rc<OpenDocument>], path: &Path) -> Option<Rc<OpenDocument>> {
    let wanted = file_tree::path_key(path);
    documents
        .iter()
        .find(|document| {
            let file = document.file.borrow();
            file.path()
                .is_some_and(|held| file_tree::path_key(held) == wanted)
        })
        .cloned()
}

/// 開いている文書の本文を置き換える。**1回のUndoで戻る**、保存はしない。
fn replace_open(
    window: &AppWindow,
    live: &Live,
    document: &Rc<OpenDocument>,
    search: &find::Search,
    replacement: &str,
) -> Result<usize, ()> {
    if document.read_only() || has_reading_view(window, live, document) {
        return Err(());
    }
    let old = document.text.borrow().clone();
    let (next, replaced) = search.replace_all(&old, replacement);
    if replaced == 0 {
        return Ok(0);
    }
    // 本文の全置換と同じく、文書の上限（要件 2.3）を越えるなら置き換えない。
    if next.chars().count() > MAX_DOCUMENT_CHARACTERS {
        return Err(());
    }
    document.history.borrow_mut().separate_next = true;
    let (at, removed, inserted) = find::changed_span(&old, &next);
    let change = Change {
        at,
        removed,
        inserted,
    };
    document.record(
        at,
        old[at..at + removed].to_owned(),
        next[at..at + inserted].to_owned(),
    );
    *document.text.borrow_mut() = next.clone();
    document.history.borrow_mut().separate_next = true;
    // draw_edit は同じ文書を見ている面をすべて動かす。キャレットを運ぶのは最初の面だけ。
    if let Some(id) = PaneId::all(window)
        .into_iter()
        .find(|id| Rc::ptr_eq(&live.states.document(*id), document))
    {
        carry_state_across(&live.states.of(id), change, &next);
        id.draw_edit(
            window,
            &live.states,
            &live.cache,
            document,
            &next,
            None,
            change,
        );
    }
    saving::write_work_copy_of(window, live, document);
    Ok(replaced)
}

#[derive(Debug, PartialEq, Eq)]
enum Refused {
    Changed,
    Other,
}

/// 開いていないファイルを、バックアップを取ってから書き換える。
fn replace_closed(
    live: &Live,
    path: &Path,
    search: &find::Search,
    replacement: &str,
    store: Option<&backup::Store>,
    keep: usize,
) -> Result<usize, Refused> {
    let source = ClosedSource::read(path)?;
    let (next, replaced) = search.replace_all(&source.text, replacement);
    if replaced == 0 {
        return Ok(0);
    }
    if next.chars().count() > MAX_DOCUMENT_CHARACTERS {
        return Err(Refused::Other);
    }
    let Some(store) = store else {
        return Err(Refused::Other);
    };
    match source.install(path, &next, store, keep) {
        Ok(retained) => {
            if let Some(retained) = retained {
                live.cache.borrow_mut().log_diag(
                    "file",
                    &format!("replace retained old file at {}", retained.display()),
                );
            }
            Ok(replaced)
        }
        Err(error) => {
            live.cache.borrow_mut().log_diag(
                "file",
                &format!("replace failed path={} error={error}", path.display()),
            );
            Err(refusal(error))
        }
    }
}

struct ClosedSource {
    file: std::fs::File,
    version: disk_entry::Version,
    text: String,
    form: file_io::TextForm,
    bytes: Vec<u8>,
}

impl ClosedSource {
    fn read(path: &Path) -> Result<Self, Refused> {
        // Deny writes to this file while reading and preparing the result.
        // Other editors can rename a new entry onto the path, so install
        // also checks the identity of the entry that would be replaced.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_DELETE).0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
            .map_err(refusal)?;
        let metadata = file.metadata().map_err(refusal)?;
        if !metadata.is_file()
            || metadata.permissions().readonly()
            || file_tree::is_reparse_point(&metadata)
        {
            return Err(Refused::Other);
        }
        let version = disk_entry::Version::of(&file).map_err(refusal)?;
        let (text, form, bytes) = file_io::read_for_replacement(&mut file, MAX_DOCUMENT_CHARACTERS)
            .map_err(|_| Refused::Other)?;
        Ok(Self {
            file,
            version,
            text,
            form,
            bytes,
        })
    }

    fn install(
        self,
        path: &Path,
        next: &str,
        store: &backup::Store,
        keep: usize,
    ) -> io::Result<Option<PathBuf>> {
        if disk_entry::Version::read(path)? != self.version {
            return Err(disk_entry::changed());
        }
        let stage = disk_entry::Stage::beside(path)?;
        let incoming = stage.incoming();
        let outcome = (|| {
            file_io::save(&incoming, next, self.form)
                .map_err(|error| io::Error::other(format!("{error:?}")))?;
            store.take_exact(path, &self.bytes, keep, timestamp::now())?;
            stage.replace(&incoming, path, self.version)
        })();
        // Release the read handle before removing the displaced entry.
        drop(self.file);
        match outcome {
            Ok(previous) => Ok(std::fs::remove_file(&previous).is_err().then_some(previous)),
            Err(error) => {
                let _ = std::fs::remove_file(incoming);
                Err(error)
            }
        }
    }
}

fn refusal(error: io::Error) -> Refused {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::NotFound
    ) || matches!(error.raw_os_error(), Some(32 | 33))
    {
        Refused::Changed
    } else {
        Refused::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_file_replaced_after_the_read_keeps_its_external_contents() {
        let root = saving::scratch_directory("replace-version");
        let path = root.join("memo.md");
        let original = root.join("previous.md");
        fs::write(&path, "置換前\r\n").unwrap();
        let source = ClosedSource::read(&path).unwrap();
        fs::rename(&path, &original).unwrap();
        fs::write(&path, "外部からの更新").unwrap();
        let store = backup::Store::new(root.join("backups"), backup::DEFAULT_NAME_FORMAT);

        let error = source.install(&path, "置換後\n", &store, 10).unwrap_err();

        assert_eq!(refusal(error), Refused::Changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "外部からの更新");
        assert_eq!(fs::read_to_string(original).unwrap(), "置換前\r\n");
    }
}
