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
    let (mut files, mut places, mut refused) = (0usize, 0usize, 0usize);
    for path in &plan.files {
        let documents = open_documents(live);
        let replaced = match held_document(&documents, path) {
            Some(document) => replace_open(window, live, &document, &search, &plan.replacement),
            None => replace_closed(live, path, &search, &plan.replacement, store.as_ref(), keep),
        };
        match replaced {
            Ok(0) => {}
            Ok(count) => {
                files += 1;
                places += count;
            }
            Err(()) => refused += 1,
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
    live.cache.borrow_mut().log_diag(
        "search",
        &format!("replace files={files} places={places} refused={refused}"),
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

/// 開いていないファイルを、バックアップを取ってから書き換える。
fn replace_closed(
    live: &Live,
    path: &Path,
    search: &find::Search,
    replacement: &str,
    store: Option<&backup::Store>,
    keep: usize,
) -> Result<usize, ()> {
    let shown = path.display();
    let readonly = std::fs::metadata(path)
        .map_err(|_| ())?
        .permissions()
        .readonly();
    if readonly {
        return Err(());
    }
    let loaded = file_io::read(path, MAX_DOCUMENT_CHARACTERS).map_err(|_| ())?;
    let (next, replaced) = search.replace_all(&loaded.text, replacement);
    if replaced == 0 {
        return Ok(0);
    }
    if next.chars().count() > MAX_DOCUMENT_CHARACTERS {
        return Err(());
    }
    let Some(store) = store else {
        return Err(());
    };
    if let Err(error) = store.take(path, keep, timestamp::now()) {
        let told = format!("replace backup failed path={shown} error={error}");
        live.cache.borrow_mut().log_diag("file", &told);
        return Err(());
    }
    if let Err(error) = file_io::save(path, &next, loaded.form) {
        let told = format!("replace write failed path={shown} error={error:?}");
        live.cache.borrow_mut().log_diag("file", &told);
        return Err(());
    }
    Ok(replaced)
}
