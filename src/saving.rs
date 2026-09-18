//! 要件 8: 書いたものを失わないための取り決め、ぜんぶ。
//!
//! 4つの約束がここに集まっている。**集めてあるのは、4つが同じ1つの資源を
//! 取り合うからである**——アプリ専用領域に置く作業コピーで、書く側（8.1）と
//! 消す側（8.2）と読み直す側（8.5の復元）が順序を間違えると、書き手の文章が
//! 消える。順序の話を1つのファイルの中でする。
//!
//! - **8.1 自動退避。**入力停止から2秒、続いていても5秒ごと。`writer.rs`の
//!   スレッドへ渡し、**失敗したら旗を立て直す**（＝2秒後にもう一度）。
//!   設定で切れる（`AUTOSAVE_SETTING`）。
//! - **8.2 明示的な保存。**`Ctrl+S`は確認なしで上書きし、外部変更と競合した
//!   ときだけ訊く。作業コピーの破棄は**書き込みと同じ待ち行列**を通る。
//! - **8.3 外部変更。**未編集の読むだけの面は読み直し、編集モードでは知らせる。
//!   作業コピーは「どの版に対して書いていたか」を持ち歩くので、閉じている
//!   あいだの変更も見分けられる。
//! - **8.4/8.5 の復元。**次の起動で作業コピーを本文へ戻す。**セッション
//!   （どのタブがどのペインに、という配置）は`main.rs`側**にある——あちらは
//!   タブとペインの話で、こちらは文章そのものの話である。
//!
//! `Live`と`AppWindow`をそのまま受け取る。ここはまだ編集器の内側で、
//! `buffer.rs`や`app_data.rs`のように外から切り離せる層ではない——切るなら
//! `Live`のほうを細くするのが先で、それは別の日の仕事である（技術検証 9.3）。

use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use crate::StatusBar;
use crate::buffer::{DocumentFile, ExternalChange};
use crate::file_io::{self, Encoding, LoadError};
use crate::i18n::pick;
use crate::open_document::OpenDocument;
use crate::say;
use crate::workspace;
use crate::{
    AUTOSAVE_SETTING, AppWindow, EditorState, Live, MAX_DOCUMENT_CHARACTERS, Opening, PaneId,
    Question, WORK_COPY_IDLE, WORK_COPY_LONGEST, WORK_COPY_SETTLE, app_data, ask_question,
    elapsed_ms, file_dialog, floor_char_boundary, focused_pane, ime, open_documents,
    open_path_in_focused_pane, publish_tabs, replace_document, shell, writer,
};

/// Whether the work copy is due (要件 8.1).
///
/// Two rules, and the second is the one that makes long typing safe: the first
/// alone would never fire while somebody keeps going, which is exactly when
/// there is the most to lose. `since_pending` is measured from the start of the
/// run of changes now waiting, not from the last copy, so the guarantee is
/// "never more than this far apart" rather than "this long after a quiet spell".
pub fn work_copy_due(since_change: Duration, since_pending: Duration) -> bool {
    since_change >= WORK_COPY_IDLE || since_pending >= WORK_COPY_LONGEST
}

/// A document's work copy with no text: enough to name the file it lives in.
pub fn work_identity(file: &DocumentFile) -> app_data::WorkCopy {
    app_data::WorkCopy {
        origin: file.path().map(Path::to_path_buf),
        untitled: file.untitled_number(),
        ..app_data::WorkCopy::default()
    }
}

/// Drop a work copy that is no longer needed (要件 8.2).
///
/// **書き込みと同じ待ち行列を通る**（2026-09-08）。退避は別スレッドで走って
/// いて、`sync_all`まで含めて数ミリ秒かかる——その場で`remove_file`すると、
/// **消したあとに古いコピーが書き上がり、次の起動で戻ってくる**。行列が1本なら
/// 順序を約束するものも1本で済み、`writer.rs`の畳み込み（同じパスは新しいほう
/// だけ残す）が「書いて、消す」を「消す」に縮めてくれる。
///
/// スレッドが無いときはその場で消す——書き込みがその場で走る道と同じ側である。
pub fn discard_work_copy(live: &Live, copy: &app_data::WorkCopy) {
    let name = app_data::work_file_name(copy);
    for document in open_documents(live) {
        if app_data::work_file_name(&work_identity(&document.file.borrow())) == name {
            document.protective_recovery.set(false);
            document.recovery_failed.set(false);
        }
    }
    let Some(directory) = app_data::work_directory() else {
        return;
    };
    let path = directory.join(app_data::work_file_name(copy));
    if live.writer.remove(path) {
        return;
    }
    let _ = app_data::discard_in(&directory, copy);
}

/// S1: closing a memo must not leave an invisible backup to restore later.
pub fn discard_memo_copy(window: &AppWindow, live: &Live, document: &Rc<OpenDocument>) -> bool {
    let Some(directory) = app_data::work_directory() else {
        return true;
    };
    let copy = work_identity(&document.file.borrow());
    let path = directory.join(app_data::work_file_name(&copy));
    let removed = if live.writer.remove(path.clone()) {
        let results = live.writer.settle(WORK_COPY_SETTLE);
        let removed = results.iter().any(|result| {
            result.path == path && result.removed && !result.superseded && result.error.is_none()
        });
        report_write_results(window, live, results);
        removed
    } else {
        app_data::discard_in(&directory, &copy).is_ok()
    };
    if !removed {
        document.text.mark_pending();
        window.tell_tab(
            say!(
                "作業コピーを削除できなかったため、メモを閉じずに残しました",
                "Could not delete the work copy, so the memo was kept open"
            )
            .into(),
        );
    }
    removed
}

/// Take away every work copy there is (追加要件 2026-09-08).
///
/// **自動退避を切った瞬間に走る。**切ったのに前の退避が残っていれば、次の
/// 起動でそれが戻ってくる——書き手は「維持しない」と言ったのに、いちばん
/// 古い姿だけが維持されることになる。開いている文書はそのまま：切るのは
/// ディスクに置くことであって、書いているものではない。
///
/// **消すのも待ち行列を通る**（追加要件 2026-09-09、残り1）。ここだけが
/// [`discard_work_copy`]を通らずその場で`remove_file`していた——退避の
/// 書き込みは別スレッドで数ミリ秒かかるので、**消したあとに古いコピーが
/// 書き上がる**。Offのあいだは復元しないので画面には出ないが、Onへ戻した
/// 次の起動でそれが本文として戻ってくる。
///
/// 数えるのは**ディスクにあるものと、開いている文書のぶんの両方**である。
/// 行列で待っているだけのコピーはまだファイルとして存在しないので
/// [`app_data::read_all_in`]には出てこない——「未完成・待機中のコピーも
/// 対象に含め」るには、文書の側から名前を作って並べるしかない。同じ名前を
/// 二度並べても`writer.rs`が畳むが、数が合わなくなるので先に落とす。
pub fn discard_all_work_copies(live: &Live) -> usize {
    let Some(directory) = app_data::work_directory() else {
        return 0;
    };
    // 名前を作るのに要るのは`work_file_name`だけで、道は
    // [`discard_work_copy`]がもう一度組み立てる。ここでフォルダを訊くのは
    // **置き場所が無ければ数える対象も無い**からである。
    let records = app_data::read_records_in(&directory);
    let documents = open_documents(live);
    let protected: std::collections::HashSet<_> = records
        .iter()
        .filter(|(_, protected)| *protected)
        .map(|(copy, _)| app_data::work_file_name(copy))
        .chain(
            documents
                .iter()
                .filter(|document| document.protective_recovery.get())
                .map(|document| app_data::work_file_name(&work_identity(&document.file.borrow()))),
        )
        .collect();
    let mut named = Vec::new();
    let mut copies = Vec::new();
    for copy in records.into_iter().map(|(copy, _)| copy).chain(
        documents
            .iter()
            .map(|document| work_identity(&document.file.borrow())),
    ) {
        let name = app_data::work_file_name(&copy);
        if protected.contains(&name) || named.contains(&name) {
            continue;
        }
        named.push(name);
        copies.push(copy);
    }
    let gone = copies.len();
    for copy in &copies {
        discard_work_copy(live, copy);
    }
    live.cache
        .borrow_mut()
        .log_diag("work", &format!("autosave off, discarded={gone}"));
    gone
}

/// Put every unsaved document back in the queue for a work copy (追加要件
/// 2026-09-09、残り1).
///
/// **自動退避を入れ直した瞬間に走る。**塞いでいるのは1つの並びだけである：
/// Offにすると[`discard_all_work_copies`]が置いてあるコピーを全部消すので、
/// **Onへ戻したあと、未保存の文書にコピーが1つも無い状態**が残りうる。
/// そのまま閉じると終了時の[`write_work_copy_now`]も旗を見て何もしないので、
/// 未保存の本文はどこにも残らない。
///
/// **打っているあいだは、これは要らない**——`SharedText::borrow_mut`が編集の
/// たびに旗を立て、[`write_work_copy_of`]はOffのあいだそれを*下ろさない*ので、
/// Onへ戻せば次の時計で書かれる。だから穴は「Offにして、**何も打たずに**、
/// Onへ戻す」ときだけ開く（Offにする直前に最後のコピーが書けていて、旗が
/// 下りていた場合）。**自動退避がOnのあいだ、未保存の文書には必ずコピーがある**
/// ——それが要件 8.1 の約束で、ここはOffが壊したその状態を戻すだけである。
///
/// 対象は**未保存の本文がある文書だけ**——`mark_pending`が`edited`を自分で
/// 見るので、ここは全部に訊いて、旗が立ったものを数えるだけでよい。保存済みの
/// 文書には失うものが無く、コピーはディスクにあるファイルと同じものになる。
/// 既に待っている文書の時計は動かさない（そちらのほうが古く、正しい）。
pub fn keep_work_copies_again(live: &Live) -> usize {
    let mut waiting = 0;
    for document in open_documents(live) {
        document.text.mark_pending();
        if document.text.pending_since().is_some() {
            waiting += 1;
        }
    }
    live.cache
        .borrow_mut()
        .log_diag("work", &format!("autosave on, waiting={waiting}"));
    waiting
}

/// Whether the settings file leaves the automatic work copies switched on.
///
/// **Asked of the file rather than the window**, because the one caller runs
/// before the settings have been applied: 要件 8.1 puts the writer's own text
/// on screen before anything else, and that is earlier in `main` than 要件 9's
/// values are read. The window is the source of truth everywhere else.
pub fn autosave_wanted() -> bool {
    let Some(directory) = app_data::app_directory() else {
        return true;
    };
    let Some(values) = app_data::read_settings(&directory) else {
        return true;
    };
    values
        .iter()
        .find(|(name, _)| name == AUTOSAVE_SETTING)
        .is_none_or(|(_, value)| value.trim() != "0")
}

/// Write the work copy if either of 要件 8.1's rules says it is time.
///
/// Jobs clear the pending flag when queued; failed jobs put it back so the
/// current text (or removal of its stale backup) can be retried.
pub fn write_work_copy_if_due(window: &AppWindow, live: &Live) {
    // Asked of every open document. Each has its own two clocks, and the one
    // that has stopped being typed in is exactly the one whose two seconds run
    // out first (要件 8.1).
    let now = Instant::now();
    for document in open_documents(live) {
        let Some(pending_since) = document.text.pending_since() else {
            continue;
        };
        let since_change = now.duration_since(document.text.changed_at());
        let since_pending = now.duration_since(pending_since);
        if work_copy_due(since_change, since_pending) {
            write_work_copy_of(window, live, &document);
        }
    }
}

/// Write every open document's work copy whatever the timing says.
///
/// Used before anything structural — a tab switch, a close — so that nothing is
/// riding on the timer across it. Does nothing for a document with nothing
/// waiting, so it is safe to call at any of them.
pub fn write_work_copy_now(window: &AppWindow, live: &Live) {
    // Every document any pane is holding, not only the focused one: two panes
    // can be looking at two files, and both of them have work to lose (要件
    // 8.1). Collected first, because writing borrows the list again.
    let open = open_documents(live);
    for document in open {
        write_work_copy_of(window, live, &document);
    }
}

/// Write one document's work copy, if it has changes waiting.
///
/// 追加要件 2026-09-08: **自動退避を切ってあれば、ここで終わる。**要件 8.1 の
/// 道はすべてこの1本を通る（時計も、タブを離れるときも、閉じるときも）ので、
/// 止める場所も1つで足りる。**待っている旗は下ろさない**——切っている間に
/// 書かれた文字は「まだ退避していない変更」のままで、書き手が入れ直せば
/// その続きから退避が始まる。
pub fn write_work_copy_of(window: &AppWindow, live: &Live, document: &Rc<OpenDocument>) {
    write_work_copy_of_impl(window, live, document, false);
}

/// Same as [`write_work_copy_of`], but **ignores the 要件 8.1 switch**
/// (`window.get_autosave()`).
///
/// Used by [`FolderAutoSave`] while a document under `SaveMode::AutoSave` is
/// blocked from writing straight to its file: the direct save is not
/// guaranteed, so the ordinary recovery copy is the fallback of last resort,
/// and it has to keep working even for a writer who turned 8.1's own recovery
/// off — they opted a folder into the *stronger* protection, not a weaker one.
fn force_work_copy_of(window: &AppWindow, live: &Live, document: &Rc<OpenDocument>) -> bool {
    if document.text.edited() {
        document.protective_recovery.set(true);
    }
    write_work_copy_of_impl(window, live, document, true)
}

fn write_work_copy_of_impl(
    window: &AppWindow,
    live: &Live,
    document: &Rc<OpenDocument>,
    force: bool,
) -> bool {
    if document.read_only() && !force {
        return false;
    }
    if !force && !window.get_autosave() && !document.protective_recovery.get() {
        return false;
    }
    if !force && document.text.pending_since().is_none() {
        return false;
    }
    let cache = &live.cache;
    let file = &document.file;
    let Some(directory) = app_data::work_directory() else {
        if !document.recovery_failed.replace(true) {
            window.tell(
                say!(
                    "作業コピーの保存先を利用できません",
                    "The work copy location is unavailable"
                )
                .into(),
            );
        }
        return false;
    };
    // Undo back to the saved text retires the old backup, including a write
    // still in flight. Do not leave its earlier contents to be restored.
    if !document.text.edited() && file.borrow().external_change() == ExternalChange::None {
        let copy = work_identity(&file.borrow());
        let path = directory.join(app_data::work_file_name(&copy));
        document.text.work_copy_written();
        if !live.writer.remove(path.clone()) {
            let error = app_data::discard_in(&directory, &copy)
                .err()
                .map(|e| e.to_string());
            report_write_results(
                window,
                live,
                vec![writer::WriteResult {
                    path,
                    bytes: 0,
                    ms: 0.0,
                    removed: true,
                    superseded: false,
                    error,
                }],
            );
        }
        document.protective_recovery.set(false);
        return true;
    }
    // The caret belongs to a pane that is showing *this* document; every pane
    // keeps its own (3.7), and only one of them can be restored into a single
    // position. The focused pane is asked first, because that is where the
    // writer is.
    let focused = focused_pane(window);
    let showing = std::iter::once(focused)
        .chain(PaneId::all(window))
        .find(|id| Rc::ptr_eq(&live.states.document(*id), document));
    let caret = showing.and_then(|id| live.states.of(id).borrow().caret_source_byte);
    let copy = app_data::WorkCopy {
        origin: file.borrow().path().map(Path::to_path_buf),
        untitled: file.borrow().untitled_number(),
        caret,
        // 要件 8.3（2026-09-08追加）: **どの版に対して書いていたか**を一緒に
        // 置く。これが無いと、次の起動で復元したとき「合意した姿」が*その時の*
        // ファイルになり、閉じているあいだの外部変更が消える。
        stamp: file.borrow().agreed_stamp(),
        text: document.text.borrow().clone(),
    };
    // Marked written as soon as it is handed over, not when it lands. The
    // alternative is to keep asking every tick until the disk answers, which
    // would queue a second copy of the same document behind the first.
    document.text.work_copy_written();
    let path = directory.join(app_data::work_file_name(&copy));
    let bytes =
        app_data::encode_with_protection(&copy, document.protective_recovery.get()).into_bytes();
    let length = bytes.len();
    document.recovery_failed.set(false);
    if live.writer.write(path.clone(), bytes) {
        cache
            .borrow_mut()
            .log_diag("work", &format!("queued bytes={length}"));
        return true;
    }
    // No writer thread. Written here instead, which is what this did before
    // the thread existed.
    let started = Instant::now();
    let outcome = std::fs::create_dir_all(&directory).and_then(|_| {
        crate::file_io::write_atomically(
            &path,
            app_data::encode_with_protection(&copy, document.protective_recovery.get()).as_bytes(),
        )
    });
    let elapsed = elapsed_ms(started);
    let success = outcome.is_ok();
    report_write_results(
        window,
        live,
        vec![writer::WriteResult {
            path,
            bytes: length,
            ms: elapsed,
            removed: false,
            superseded: false,
            error: outcome.err().map(|error| error.to_string()),
        }],
    );
    success
}

/// Write the last work copies and **wait to hear whether they landed**
/// (追加要件 2026-09-09、残り2).
///
/// 終了の直前に呼ぶ。それまで、最後の退避は行列に乗せるだけで、書けたかどうかは
/// `finish`のあと——**窓が閉じたあと**——にしか分からなかった。ディスクが一杯でも
/// 書き込み権が無くても、書き手には最後の入力を失ったことを知る道が無い。
///
/// 返すのは**書けなかった作業コピーの数**。0なら何も起きなかったのと同じで、
/// 問いは増えない（要件 8.1 は静かな約束である）。
pub fn flush_work_copies(window: &AppWindow, live: &Live) -> usize {
    write_work_copy_now(window, live);
    let landed = live.writer.settle(WORK_COPY_SETTLE);
    report_write_results(window, live, landed)
}

/// Log what the writer thread has finished since last asked.
///
/// Polled on the same timer that decides when to write, so nothing on that
/// thread has to reach into the UI and no lock is shared with it.
pub fn collect_write_results(window: &AppWindow, live: &Live) {
    report_write_results(window, live, live.writer.drain());
}

/// Log and act on a batch of results the writer thread has handed back, and
/// say **how many of them were work copies that could not be written**
/// (追加要件 2026-09-09、残り2).
///
/// Split out from [`collect_write_results`] because the close path collects its
/// results a different way — it waits for them ([`writer::FileWriter::settle`])
/// rather than taking whatever has landed — and everything that happens to a
/// result afterwards has to be the same either way: the flag put back up, the
/// line in the log, the message on screen.
///
/// **削除の失敗は数に入らない。**片づけられなかったコピーは書き手の文章を
/// 失わせない——残るだけである。数えるのは失われうるものだけ。
pub fn report_write_results(
    window: &AppWindow,
    live: &Live,
    results: Vec<writer::WriteResult>,
) -> usize {
    let mut lost = 0;
    if results.is_empty() {
        return 0;
    }
    for result in results {
        // **置き換えられた仕事は、何もしなかった仕事である**（2026-09-09）。
        // 同じ文書の新しいコピーが先に行列へ入っただけなので、旗も画面も
        // 触らない。ログには残す——順序の話はここでしか読めない。
        if result.superseded {
            let shown = result.path.display().to_string();
            live.cache
                .borrow_mut()
                .log_diag("work", &format!("superseded path={shown}"));
            continue;
        }
        if result.error.is_some() && !result.removed {
            lost += 1;
        }
        let shown = result.path.display().to_string();
        let message = match (&result.error, result.removed) {
            (None, false) => format!(
                "saved bytes={} ms={:.2} path={shown}",
                result.bytes, result.ms
            ),
            (None, true) => format!("discarded ms={:.2} path={shown}", result.ms),
            (Some(error), removed) => {
                format!("failed removed={} error={error}", u8::from(removed))
            }
        };
        live.cache.borrow_mut().log_diag("work", &message);
        if result.error.is_none() {
            continue;
        }
        // 2026-09-08: **失敗したら旗を立て直す。**渡した時点で「退避済み」に
        // していたので、書けなかった一回はそのまま忘れられていた——次の打鍵が
        // 無ければ、その文書は二度と退避されない（要件 8.1 が守れていない）。
        // `retry_work_copy`は削除の失敗も再試行の対象にする。
        //
        // **新しい編集が来ていれば何もしない**（`retry_work_copy`は待っている
        // 旗があれば触らない）。そちらの時計のほうが正しい。
        let named = result.path.file_name().and_then(|name| name.to_str());
        let failed = open_documents(live).into_iter().find(|document| {
            let copy = work_identity(&document.file.borrow());
            named == Some(app_data::work_file_name(&copy).as_str())
        });
        if let Some(document) = failed {
            document.text.retry_work_copy();
            document.recovery_failed.set(true);
        }
        // **画面にも出す。**要件 8.1 は書き手への約束なので、守れていないことは
        // 書き手が知っていなければならない。1件目だけ——同じ理由で失敗した
        // 数件が順に上書きし合っても、読めるのは最後の1つである。
        let told = if result.removed {
            pick(
                "作業コピーを片づけられませんでした",
                "Could not clear the work copy",
            )
        } else {
            pick(
                "作業コピーを退避できませんでした。まもなく再試行します",
                "Could not back up to the work copy. Trying again shortly",
            )
        };
        window.tell(told.into());
    }
    lost
}

/// Notice another program writing the file (要件 8.3).
///
/// **A document with nothing unsaved is reloaded without asking.** That is what
/// the writer would do by hand, and there is nothing of theirs to lose. An
/// edited one is only reported: either side could be the one worth keeping, and
/// choosing without being told is how work disappears.
pub fn check_external_change(window: &AppWindow, live: &Live) {
    // **開いている文書を全部見る**（書き手のレビュー 2026-09-11、S2）。前にある
    // 文書だけを見ていると、**後ろのタブで起きた変更は、そのタブへ移るまで誰も
    // 気づかない**——戻ったときには、書き手はもうそのファイルのことを忘れている。
    check_documents(window, live, crate::open_documents(live));
}

/// 同じ見回りを、**ReadOnlyで前に出ている文書だけ**に（追加要件 2026-09-15、書き手の選択）。
///
/// 0.5秒ごとに来る。ReadOnlyは書き手が編集しないので、変われば必ず読み直しになる
/// ——流れているログがそのまま流れて見える。何も開いていなければ何もしない。
pub fn check_read_only_change(window: &AppWindow, live: &Live) {
    let reading = crate::read_only_documents(window, live);
    if !reading.is_empty() {
        check_documents(window, live, reading);
    }
}

fn check_documents(window: &AppWindow, live: &Live, documents: Vec<Rc<OpenDocument>>) {
    let active = live.active(window);
    // **何も起きていなければ、画面に触らない。**この見回りは2秒ごとに来るので、
    // 毎回タブを組み直すと、何事もない時間のほうが高くつく。
    let mut noticed = false;
    for document in documents {
        let file = &document.file;
        let change = file.borrow().external_change();
        if change == ExternalChange::Missing {
            if !document.missing.replace(true) {
                document.outside.set(true);
                noticed = true;
                if Rc::ptr_eq(&document, &active) {
                    window
                        .tell_tab(say!(
                            "ファイルが見つからないため、外部版とは比較できません。本文は保持しています",
                            "The file is missing, so it cannot be compared. The text is kept"
                        ).into());
                }
            }
            continue;
        }
        if document.missing.replace(false) {
            noticed = true;
            document.outside.set(change == ExternalChange::Modified);
        }
        if change != ExternalChange::Modified {
            continue;
        }
        let Some(stamp) = file.borrow().current_stamp() else {
            continue;
        };
        if !file.borrow_mut().take_report(stamp) {
            continue;
        }
        // **読むだけの面（ReadOnly・Viewer）で開いていて、失うものが無ければ取り込む**
        // （要件 8.3、書き手の判断 2026-09-15）。前にある文書でなくても同じである。
        if !document.text.edited() && crate::has_reading_view(window, live, &document) {
            noticed = true;
            reload_document(window, live, &document);
            continue;
        }
        // **編集モードでは取り込まない**（書き手の判断 2026-09-15：「警告を出したら、
        // 読み直す指示があるまで外部からの取り込みの更新は止めていてもいい」）。
        // 書いている面の下で本文が替わると、読んでいた場所も見失う。未編集でも同じで、
        // 読み直すかReadOnlyで読むかは印から訊く（`ask_outside_change`）。
        //
        // **知らせは1度だけ**：書き足され続けるログは2秒ごとに印を立て直すが、
        // 印は片付くまで消えないので（S2）、立っているあいだは黙っている。
        if document.outside.replace(true) {
            continue;
        }
        noticed = true;
        if Rc::ptr_eq(&document, &active) {
            window.tell_tab(
                say!(
                    "別のアプリがこのファイルを変更しました。「⚠ 外で変更」から読み直せます",
                    "Another app changed this file. Reload it from \"⚠ Changed outside\""
                )
                .into(),
            );
        }
        live.cache.borrow_mut().log_diag(
            "external",
            &format!(
                "modified edited={} active={} action=mark",
                u8::from(document.text.edited()),
                u8::from(Rc::ptr_eq(&document, &active))
            ),
        );
    }
    if noticed {
        crate::publish_tabs(window, live);
        crate::publish_active_encoding(window, live);
    }
}

/// Take the file as it now is, in place of what the editor holds (要件 8.3).
///
/// **Also the answer that throws work away**, when it is chosen from the
/// conflict question rather than reached with nothing unsaved. The work copy
/// held exactly what is being given up, so it goes too — left behind, it would
/// bring the discarded text back at the next start.
pub fn reload_from_file(window: &AppWindow, live: &Live) {
    reload_document(window, live, &live.active(window));
}

/// 同じことを、**どの文書かを言われて**する（書き手のレビュー 2026-09-11、S2）。
///
/// **前にある文書とは限らない**——後ろのタブで起きた外部変更も、失うものが無ければ
/// そこで読み直す（要件 8.3）。
pub fn reload_document(window: &AppWindow, live: &Live, document: &Rc<OpenDocument>) {
    if document.read_only() {
        return;
    }
    let reloaded = document.file.borrow_mut().reload(MAX_DOCUMENT_CHARACTERS);
    match reloaded {
        Some(Ok(text)) => {
            let bytes = text.len();
            replace_document(window, &live.states, &live.cache, document, text);
            // Reloaded, not edited: the text and the file agree by definition.
            document.text.mark_saved();
            // **片付いたので、印は下りる**（書き手のレビュー 2026-09-11、S2）。
            document.outside.set(false);
            document.missing.set(false);
            discard_work_copy(live, &work_identity(&document.file.borrow()));
            publish_tabs(window, live);
            window.tell_tab(say!("外部の変更を読み込みました", "Loaded the outside change").into());
            live.cache
                .borrow_mut()
                .log_diag("external", &format!("reloaded bytes={bytes}"));
        }
        Some(Err(error)) => {
            window.tell_tab(say!("読み直せません: {error}", "Cannot reload: {error}").into());
            live.cache
                .borrow_mut()
                .log_diag("external", &format!("reload failed error={error}"));
        }
        None => {}
    }
}

/// この文書を、言われた文字コードで開き直す（要件 E2 の②）。
///
/// **未保存の本文があれば、先に訊く**のは呼ぶ側（`main.rs`の`reopen_as_asked`）で、
/// ここへ来るのは訊き終わったあとである。ここがするのは読み直しそのものだけ。
///
/// **読めなければ何もしない。**文書は読めていたときのままで、ファイルにも触って
/// いない——開き直しは、失敗しても何も失わない操作である。
pub fn reopen_as(window: &AppWindow, live: &Live, document: &Rc<OpenDocument>, encoding: Encoding) {
    if document.read_only() {
        return;
    }
    // **いま何で読んでいるか**を、読み直す前に控える。同じものを選んだのなら
    // 字は1つも変わらない——書き手の報告 2026-09-10：「特に壊れて見えません」は
    // **UTF-16 BEの見本をUTF-16 BEで開き直した**回で、答えとしては正しいのに
    // 「開き直しました」としか言わなかったので、効かなかったのと区別が付かなかった。
    let held = document.file.borrow().form().encoding;
    let reopened = document
        .file
        .borrow_mut()
        .reopen_as(MAX_DOCUMENT_CHARACTERS, encoding);
    let name = encoding.as_str();
    match reopened {
        Some(Ok(text)) => {
            let bytes = text.len();
            let mixed = document.file.borrow().mixed_newlines();
            replace_document(window, &live.states, &live.cache, document, text);
            // 開き直したのだから、本文とファイルは定義により一致している
            // ——退避も、いま捨てた本文のぶんは要らない。
            document.text.mark_saved();
            discard_work_copy(live, &work_identity(&document.file.borrow()));
            publish_tabs(window, live);
            // **揃えたことは言う**（読み込みと同じ規則）。混ざった改行は
            // 1つへ揃えてあるので、黙っていると保存で揃ったことが画面の
            // どこにも出ない。
            // **同じ文字コードなら「そのまま」と言う。**選んだ行に既に印が
            // 付いていたのだから、書き手が知りたいのは「字が変わらなかったのは
            // 効かなかったからではない」ことである。
            let done = if held == encoding {
                say!(
                    "{name}のまま読み直しました（字は変わりません）",
                    "Reloaded as {name} (the text is unchanged)"
                )
            } else {
                say!("{name}で開き直しました", "Reopened as {name}")
            };
            let told = if mixed {
                say!(
                    "{done}（改行コードは混在していました）",
                    "{done} (the line breaks were mixed)"
                )
            } else {
                done
            };
            window.tell_tab(told.into());
            live.cache.borrow_mut().log_diag(
                "encoding",
                &format!(
                    "reopened as={name} was={} bytes={bytes} mixed={}",
                    held.as_str(),
                    mixed as u8
                ),
            );
        }
        Some(Err(error)) => {
            // **その文字コードでは読めなかった**のか、大きすぎたのか。前者は
            // 判別の言葉（「どれとしても読めない」）では嘘になるので、ここで
            // 言い直す——書き手が選んだのは1つの文字コードである。
            let told = match error {
                LoadError::Unreadable => {
                    say!(
                        "{name}としては読めません。文書はそのままです",
                        "Cannot read it as {name}. The document is unchanged"
                    )
                }
                other => say!("開き直せません: {other}", "Cannot reopen: {other}"),
            };
            window.tell_tab(told.into());
            live.cache
                .borrow_mut()
                .log_diag("encoding", &format!("refused as={name}"));
        }
        None => {}
    }
}

/// 名前を付けて保存の欄に出すもの（要件 E2 の③⑤）。
///
/// **初めから選ばれているのは、いまこの文書が持っている形**——上書きの`Ctrl+S`が
/// 何も訊かないのと同じ約束で、決め直さなければ元の形式のまま書かれる
/// （要件 E2：「無指定の保存では元の形式を維持する」）。
fn save_fields(held: file_io::TextForm) -> file_dialog::SaveFields<'static> {
    file_dialog::SaveFields {
        encodings: &crate::SAVE_FORM_LABELS,
        encoding: crate::save_form_id(held),
        newlines: &crate::NEWLINE_LABELS,
        newline: crate::newline_id(held.newline),
    }
}

/// The tabs left by the last run (要件 8.1, 8.4).
///
/// Each work copy holds the text; the file it belongs to holds the shape to
/// write it back in, so the original is opened for that and the text it returns
/// used as the save baseline only when its stamp still matches the backup.
/// Missing originals keep their paths, so separate documents keep separate
/// backup identities and the normal missing-file protection still applies.
pub fn restore_tabs(window: &AppWindow) -> Vec<(Rc<OpenDocument>, EditorState)> {
    // 追加要件 2026-09-08: 自動退避を切ってあれば、戻すものは無い。切った
    // ときに全部消しているので普段はここに何も残っていないが、**設定ファイル
    // を手で書き換えた場合は残っている**——そのときも、切ってあると言われた
    // なら戻さない。
    let ordinary_recovery = autosave_wanted();
    let Some(directory) = app_data::work_directory() else {
        return Vec::new();
    };
    let mut tabs = Vec::new();
    for (copy, protected) in app_data::read_records_in(&directory) {
        if !ordinary_recovery && !protected {
            continue;
        }
        let untitled = copy.untitled.max(1);
        let (file, saved_text) = match &copy.origin {
            Some(path) => match DocumentFile::open(path, MAX_DOCUMENT_CHARACTERS) {
                Ok((file, text)) => {
                    let saved =
                        (copy.stamp.is_some() && copy.stamp == file.agreed_stamp()).then_some(text);
                    (file, saved)
                }
                Err(_) => (DocumentFile::unavailable(path.clone(), copy.stamp), None),
            },
            None => (DocumentFile::untitled(untitled), Some(String::new())),
        };
        // Rounded here, because the copy was written by another run and nothing
        // guarantees the text is the same length now (6.7's rule, applied
        // across runs rather than across panes).
        let caret = copy.caret.map(|byte| floor_char_boundary(&copy.text, byte));
        let state = EditorState {
            caret_source_byte: caret,
            selection_anchor_source_byte: caret,
            ..EditorState::default()
        };
        let document = OpenDocument::new(file, copy.text, window.as_weak());
        document.protective_recovery.set(protected);
        // 要件 8.3（2026-09-08追加）: **退避した時点の姿へ戻す。**`open`は
        // いま読んだファイルの姿を「合意した姿」として持っているので、その
        // ままでは閉じているあいだに別のアプリが書き換えていても
        // `external_change`が「変わっていない」と答え、`Ctrl+S`が要件 8.3 の
        // 問いを出さずに上書きしてしまう。持ち歩いてきた姿を入れ直せば、
        // **食い違いは復元したその瞬間から見える**。
        //
        // 古いコピー（この版より前に書かれたもの）は`None`で、そのときは
        // 今までどおり——比べる相手が無いのだから、無いなりに振る舞う。
        if let Some(stamp) = copy.stamp {
            document.file.borrow_mut().agreed_at(stamp);
        }
        // Restored from a work copy: the text does not agree with its file, but
        // the copy on disk already holds it, so nothing is waiting to be
        // written.
        document.text.mark_restored(saved_text);
        let change = document.file.borrow().external_change();
        document.missing.set(change == ExternalChange::Missing);
        document.outside.set(change != ExternalChange::None);
        tabs.push((document, state));
    }
    tabs
}

/// Save the document, asking for a name only when it has none.
///
/// `ask_for_name` is 名前を付けて保存. Without it, a document that already has
/// a file is written straight over it with no confirmation — 要件 8.2 asks for
/// one only when the file has changed underneath.
///
/// **No borrow is held across a dialog.** Both of them run their own message
/// loop, and Slint goes on delivering events from inside it.
pub fn save_document(window: &AppWindow, live: &Live, ask_for_name: bool) {
    let document = live.active(window);
    if !ask_for_name && document.file.borrow().external_change() == ExternalChange::Missing {
        crate::ask_missing_file(window, live, &document);
        return;
    }
    if document.read_only() {
        window.tell_tab(
            say!(
                "外部版は読み取り専用です。必要な内容を元のタブへコピーしてください",
                "The outside version is read-only. Copy what you need into the original tab"
            )
            .into(),
        );
        return;
    }
    // 追加要件 2026-09-15: **ReadOnlyの上書きは断る。**書き足され続けるファイルへ、
    // 少し前に読んだ断面を書き戻すことになる——その間に足された行が消える。
    // 断面として残したいなら、別名で保存する（そのときReadOnlyは解ける）。
    if !ask_for_name && focused_pane(window).reads_only(window) {
        window.tell_tab(
            say!(
                "ReadOnlyモードでは上書き保存しません。残すときは別名で保存してください",
                "ReadOnly mode does not overwrite. Use Save As to keep a copy"
            )
            .into(),
        );
        return;
    }
    // **求めが届いたことを、まず残す**（書き手の報告 2026-09-10：「縦書きだと
    // 警告が出ていません」）。**鍵が届かなかった回は、ログのどこにも出ない**
    // ——書けたか断られたかの行しか無ければ、「効かなかった」と「届いていない」を
    // 見分けられない。面の向きも一緒に書くのは、報告がその違いだったからである。
    live.cache.borrow_mut().log_diag(
        "file",
        &format!(
            "save asked pane={} vertical={} name={}",
            focused_pane(window).log_name(),
            u8::from(focused_pane(window).vertical(window)),
            u8::from(ask_for_name)
        ),
    );
    let file = &document.file;
    let owner = ime::window_handle(window);
    let existing = file.borrow().path().map(Path::to_path_buf);
    let suggested = file.borrow().title();
    // 要件 E2 の③: **書く文字コードは、行き先と一緒に決まる**（書き手の判断
    // 2026-09-10：「コードを替えて保存したければ、Save Asからコード選択して
    // 保存するべき」）。**上書きの`Ctrl+S`は訊かない**——そのときは何も決め直して
    // いないので、この文書がいま持っている形で書く。
    let held = file.borrow().form();
    let (target, form) = if ask_for_name || existing.is_none() {
        let Some(chosen) = file_dialog::save_document_as(owner, &suggested, save_fields(held))
        else {
            return;
        };
        (
            chosen.path,
            crate::save_form_of_id(chosen.encoding, chosen.newline, held),
        )
    } else {
        let Some(path) = existing.clone() else {
            return;
        };
        (path, held)
    };
    // 書き手のレビュー 2026-09-11（P1）: **同じファイルに、別の本文を2つ作らない。**
    // 要件 7.6 は「同じファイルは1つの文書」と言っている——名前を付けて保存の宛先を
    // 別のタブが開いていると、**同じ保存先と同じ退避先を、別々の本文が使う**ことに
    // なる（相手の未保存があれば、退避の削除まで干渉する）。
    if let Some(other) = crate::document_at(live, &target)
        && !Rc::ptr_eq(&other, &document)
    {
        // **捨てる前に訊く。**相手のタブが未保存の仕事を抱えているなら、この保存は
        // それを上書きする。
        if other.text.edited() {
            let title = other.file.borrow().title();
            crate::ask_question(
                window,
                live,
                Question::SaveOverOpen {
                    path: target.clone(),
                    form,
                },
                say!(
                    "「{title}」は別のタブで編集中です。\n\nそちらの保存していない変更は失われます。",
                    "\"{title}\" is being edited in another tab.\n\nIts unsaved changes will be lost."
                ),
                &[
                    pick("保存する", "Save"),
                    pick("別の名前で", "Save As"),
                    pick("やめる", "Cancel"),
                ],
                2,
            );
            return;
        }
        save_over_open(window, live, &document, &other, target, form);
        return;
    }
    // 要件 8.2: the ordinary Ctrl+S is silent, and the one thing it stops for
    // is a file that has changed underneath since it was opened. 要件 8.3 gives
    // that four answers, so the writing waits for one.
    let outside_change =
        file.borrow().external_change() == ExternalChange::Modified || document.outside.get();
    if Some(&target) == existing.as_ref() && outside_change {
        let title = file.borrow().title();
        ask_question(
            window,
            live,
            Question::SaveConflict {
                path: target.clone(),
                form,
            },
            say!(
                "「{title}」は別のアプリで変更されています。\n\n\
                 読み込むと、保存していない変更は失われます。",
                "\"{title}\" was changed by another app.\n\n\
                 Loading it loses your unsaved changes."
            ),
            &conflict_choices(),
            1,
        );
        return;
    }
    if write_document_in(window, live, &document, target, form) && form != held {
        // **形が変わったことは言う。**ステータスバーの`CP932・CRLF`もそう変わるが、
        // 書き手が欄で選んだ結果がそのとおりになったことは、言葉でも1度出す。
        let mark = if form.byte_order_mark && form.encoding == Encoding::Utf8 {
            " BOM"
        } else {
            ""
        };
        // **帯と同じ言葉で言う**（`CP932・CRLF`）——欄が2つになったので、
        // 文字コードだけを言うと、改行を選び直した書き手には何も答えていない
        // ことになる（要件 E2 の⑤）。
        let told = say!(
            "{}{mark}・{}で保存しました",
            "Saved as {}{mark}・{}",
            form.encoding.as_str(),
            crate::newline_name(form.newline)
        );
        window.tell_tab(told.into());
        live.cache.borrow_mut().log_diag(
            "encoding",
            &format!(
                "save as={}{mark}・{} was={}・{}",
                form.encoding.as_str(),
                crate::newline_name(form.newline),
                held.encoding.as_str(),
                crate::newline_name(held.newline)
            ),
        );
    }
}

/// 名前を付けて保存の宛先を、別のタブが開いていた（書き手のレビュー 2026-09-11、P1）。
///
/// **書いてから、1つの文書へ合流させる**（要件 7.6：同じファイルは1つの文書）。
/// 相手のタブは残り、**保存した本文を見る**ようになる——タブは2つ、文書は1つである。
/// 合流しないと、**同じ保存先と同じ退避先を別々の本文が使う**ことになり、次にどちらかが
/// 保存した拍子に、もう一方の原稿が消える。
pub fn save_over_open(
    window: &AppWindow,
    live: &Live,
    document: &Rc<OpenDocument>,
    other: &Rc<OpenDocument>,
    target: PathBuf,
    form: file_io::TextForm,
) {
    // **書けなければ何も動かさない。**相手のタブも、相手の退避もそのままである
    // ——保存は失敗しうる操作で、失敗したときに失われるものがあってはならない。
    if !write_document_in(window, live, document, target, form) {
        return;
    }
    // 相手の退避は`write_document_in`が捨てている（保存先が同じなので、書いたあとの
    // この文書の退避先が、そのまま相手の退避先である）。
    crate::merge_documents(window, live, other, document);
}

/// Write the document into a path that has already been decided.
///
/// **Which document is an argument**: 全て保存 writes documents that are not in
/// front of any pane, so this cannot be the one the writer is looking at.
pub fn write_document_to(
    window: &AppWindow,
    live: &Live,
    document: &Rc<OpenDocument>,
    target: PathBuf,
) -> bool {
    let form = document.file.borrow().form();
    write_document_in(window, live, document, target, form)
}

/// 同じことを、**書く形を言われて**する（要件 E2 の③）。
///
/// `form`はこの保存で使う文字コード・BOM・改行で、**書けたらそれがこの文書の形に
/// なる**。表せない字があれば断る——**替えるのは本文の側**（`replace_unmappable`）で、
/// ここは替わったあとの本文を普通に書くだけである。
pub fn write_document_in(
    window: &AppWindow,
    live: &Live,
    document: &Rc<OpenDocument>,
    target: PathBuf,
    form: file_io::TextForm,
) -> bool {
    if document.read_only() {
        return false;
    }
    let cache = &live.cache;
    let file = &document.file;
    let text = document.text.borrow().clone();
    let bytes = text.len();
    let shown = target.display().to_string();
    // Taken before the save, because 名前を付けて保存 moves the document to
    // another file and the copy on disk is still under the old name.
    let previous = work_identity(&file.borrow());
    let moved = file.borrow().path() != Some(target.as_path());
    let saved_to = target.clone();
    let outcome = file.borrow_mut().save_to_as(target, &text, form);
    match outcome {
        Ok(()) => {
            document.history.borrow_mut().separate_next = true;
            document.text.mark_saved();
            // **書けば片付く**（書き手のレビュー 2026-09-11、S2）。いま書いたものが
            // そのファイルの中身で、外の版はもう無い。
            document.outside.set(false);
            document.missing.set(false);
            discard_work_copy(live, &previous);
            discard_work_copy(live, &work_identity(&file.borrow()));
            // The name in the strip changes with 名前を付けて保存, and the
            // unsaved marker changes with every save.
            publish_tabs(window, live);
            // 要件 E2: **帯は、書いた形をすぐ言う**（書き手の報告 2026-09-10）。
            // 打鍵で組み直すまで待たない——保存は本文を1字も動かさないので、
            // その組み直しは来ない。
            crate::publish_active_encoding(window, live);
            window.tell_tab(say!("保存しました", "Saved").into());
            if moved && crate::release_read_only(window, live, document) {
                window.tell_tab(
                    say!(
                        "別名で保存しました。ReadOnlyモードを解除しました",
                        "Saved under a new name. ReadOnly mode is off"
                    )
                    .into(),
                );
            }
            // 単語チェックモード要件 5.4（2026-09-08）: **保存されたのが辞書
            // そのものなら、そこから読み直す。**書き手が直したのは表であって、
            // 画面の表と食い違ったまま進むと、次に語を1つ足した拍子に書き手の
            // 編集が消える。
            if crate::is_word_file(&saved_to) {
                crate::adopt_word_file(window, live);
            }
            cache.borrow_mut().log_diag(
                "file",
                &format!(
                    // **改行の形も書く**（2026-09-12）。文字コードだけでは、
                    // 保存が原稿の改行をどう書いたかが記録に残らない——帯と
                    // 同じ言葉（`UTF-8・CRLF`）で残す。
                    "save ok bytes={bytes} as={}・{} path={shown}",
                    form.encoding.as_str(),
                    crate::newline_name(form.newline)
                ),
            );
            true
        }
        // 要件 E2 の③: **断って、次にすることを言う**（書き手の判断 2026-09-10）。
        //
        // 数えない。3万字のうち1000字が入らないとしても、その数は書き手の役に
        // 立たない——答えは「この文字コードでは保存できない」で、次にすることは
        // UTF-8で保存することである。**その道はすぐ隣にある**（同じ帯の一覧の
        // 下の節）ので、言葉でそこを指す。
        //
        // **1字だけ見せる**のは「どこを直せばいいか」の取っ掛かりで、原稿を
        // 直して済ませたい書き手のためである。
        Err(file_io::SaveError::Unmappable(_)) => {
            // **どの字かは言わない**（書き手の判断 2026-09-10：「一文字に限らない
            // ので」）。1字だけ挙げれば、それを直せば済むように読める——実際には
            // 次の字でまた断られる。書き手が次にすることは**UTF-8で保存する**で
            // あって、字を1つずつ潰していくことではない。
            //
            // 文字そのものは診断時も残さない。文字コードと失敗種別を記録する。
            window.tell_tab(
                say!(
                    "{}では表せない文字があるため保存できません。UTF-8で保存してください",
                    "Cannot save: some characters cannot be written in {}. Save as UTF-8",
                    form.encoding.as_str()
                )
                .into(),
            );
            cache.borrow_mut().log_diag(
                "encoding",
                &format!("unmappable as={} path={shown}", form.encoding.as_str()),
            );
            false
        }
        Err(error) => {
            window.tell_tab(say!("保存できません: {error}", "Cannot save: {error}").into());
            cache
                .borrow_mut()
                .log_diag("file", &format!("save failed path={shown} error={error}"));
            false
        }
    }
}

/// Write every open document that has unsaved work (要件 8.2).
///
/// **The files first, then the names.** Everything that already knows where it
/// goes is written without a word; a document with no file needs somewhere to
/// go, and it is asked for afterwards so that the writing is not held up behind
/// a dialog. **The first キャンセル ends the asking**, the way it ends a run of
/// closes: the remaining 無題 keep their work copies (要件 8.1) and their
/// unsaved marks.
///
/// One kind is left where it is: a document another program has changed since it
/// was opened. That is 要件 8.3's four-way question, and it is asked about one
/// document at a time by `Ctrl+S` — silently overwriting it here is exactly what
/// 要件 8.3 exists to prevent. The status bar says how many were left.
pub fn save_all(window: &AppWindow, live: &Live) {
    let owner = ime::window_handle(window);
    save_all_with_choice(window, live, |document| {
        let held = document.file.borrow().form();
        let suggested = document.file.borrow().title();
        let chosen = file_dialog::save_document_as(owner, &suggested, save_fields(held))?;
        Some((
            chosen.path,
            crate::save_form_of_id(chosen.encoding, chosen.newline, held),
        ))
    });
}

/// Keep the batch rules identical for the native dialog and recovery tests.
pub(crate) fn save_all_with_choice(
    window: &AppWindow,
    live: &Live,
    mut choose: impl FnMut(&Rc<OpenDocument>) -> Option<(PathBuf, file_io::TextForm)>,
) {
    let mut saved = 0;
    let mut failed = 0;
    let mut conflicted = 0;
    let mut occupied = 0;
    let mut unnamed: Vec<Rc<OpenDocument>> = Vec::new();
    for document in open_documents(live) {
        if !document.text.edited() {
            continue;
        }
        let path = document.file.borrow().path().map(Path::to_path_buf);
        let Some(path) = path else {
            unnamed.push(document);
            continue;
        };
        if document.file.borrow().external_change() != ExternalChange::None
            || document.outside.get()
        {
            conflicted += 1;
            continue;
        }
        // **ここでは問わない。**一度に何枚も書く道で問いを立てると、答える
        // まで残りが止まる——表せない字があった文書は「保存できなかった1件」
        // として数え、書き手が`Ctrl+S`で1枚ずつ選べばよい（その道が③の問い）。
        if write_document_to(window, live, &document, path) {
            saved += 1;
        } else {
            failed += 1;
        }
    }
    let mut left = 0;
    let mut stopped = false;
    for document in unnamed {
        if stopped {
            left += 1;
            continue;
        }
        let Some((target, form)) = choose(&document) else {
            stopped = true;
            left += 1;
            continue;
        };
        // A batch cannot ask a second asynchronous question and continue
        // saving. Defer collisions to the individual Save As flow, which
        // confirms replacement and merges the documents after success.
        if crate::document_at(live, &target).is_some_and(|other| !Rc::ptr_eq(&other, &document)) {
            occupied += 1;
            continue;
        }
        if write_document_in(window, live, &document, target, form) {
            saved += 1;
        } else {
            failed += 1;
        }
    }
    // Written last, over whatever the individual saves said: the count is the
    // answer to 全て保存, and one of the writes saying 保存しました is not.
    let mut told = say!("{saved}件を保存しました", "Saved {saved}");
    if failed > 0 {
        told.push_str(&say!(
            "／{failed}件は保存できません",
            " / {failed} could not be saved"
        ));
    }
    if left > 0 {
        told.push_str(&say!(
            "／無題{left}件は保存していません",
            " / {left} untitled not saved"
        ));
    }
    if conflicted > 0 {
        told.push_str(&say!(
            "／外部変更{conflicted}件は個別に保存してください",
            " / {conflicted} changed outside: save them one by one"
        ));
    }
    if occupied > 0 {
        told.push_str(&say!(
            "／{occupied}件は保存先を別のTABで開いているため保留しました。個別に名前を付けて保存してください",
            " / {occupied} deferred: destination is open in another tab. Use Save As individually"
        ));
    }
    window.tell(told.clone().into());
    live.cache.borrow_mut().log_diag("file", &told);
}

/// Show the document in front of the pane in Explorer (要件 5.2).
///
/// The same command the tree has, from the pane's own menu, because the file
/// the writer means is at least as often the one they are editing as the one
/// they have selected in the tree.
pub fn reveal_active_document(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    let path = document.file.borrow().path().map(Path::to_path_buf);
    let Some(path) = path else {
        window.tell_tab(
            say!(
                "まだ保存していない文書です",
                "This document has not been saved yet"
            )
            .into(),
        );
        return;
    };
    let shown = path.display().to_string();
    live.cache
        .borrow_mut()
        .log_diag("file", &format!("reveal path={shown}"));
    shell::reveal(&path);
}

/// Open a file the writer chooses (要件 6.5).
///
/// 2026-09-08: **入口は1つ**（[`open_path_in_pane`]）。ダイアログだけが自分で
/// `DocumentFile::open`を呼んでいて、**同じファイルを開くたびに別の
/// `OpenDocument`ができていた**——要件 7.6 は「何枚の面から見ても本文は1つ」と
/// 言っているのに、ダイアログから開いた双子どうしは互いの編集を知らず、
/// 退避先（`work_file_name`はパスから決まる）まで取り合っていた。ツリーと
/// コマンドラインが通っていた道をこちらも通る。
pub fn open_document(window: &AppWindow, live: &Live) {
    let owner = ime::window_handle(window);
    let Some(path) = file_dialog::open_document(owner) else {
        return;
    };
    // **`Kept`**：書き手が名前で指した1件で、一覧を歩いているのではない。
    open_path_in_focused_pane(window, live, &path, Opening::Kept);
}

/// 外で変わったファイルへ保存しようとしたときの選択肢（要件 8.2）。`main.rs`の問いと同じ並び。
pub fn conflict_choices() -> [&'static str; 5] {
    [
        pick("作業中の内容で上書き", "Overwrite with My Changes"),
        pick("外部の変更を読み込む", "Load the Outside Change"),
        pick("別名で保存", "Save As"),
        pick("外部版と比べる", "Compare with Outside Version"),
        pick("キャンセル", "Cancel"),
    ]
}

/// Direct-to-disk save for documents under a folder registered with
/// [`workspace::SaveMode::AutoSave`] (Workspace設計.md フェーズ4).
///
/// **Owned by whatever runs the main timer**, one instance for the run — not a
/// global. It carries its own idea of which documents are enrolled (a `Weak`
/// per document, so a closed document is never kept alive by this and a
/// reused pointer never lands on a stale entry), and asks for the
/// [`workspace::Registry`] fresh on every [`tick`](Self::tick) rather than
/// holding one, since the active Workspace can change without this being
/// told and a folder's own mode is shared across every Workspace anyway
/// (`Registry::save_mode_for`).
///
/// **Nothing here decides when a timer fires.** `tick` is meant to be called
/// from the same clock 要件 8.1 already uses; wiring that call, and exposing
/// [`status`](Self::status) to the UI, is for whoever builds the timer
/// closure this lives in.
///
/// **Integration must also call [`arm`](Self::arm)** the instant a document
/// opens under an `AutoSave` folder, or the instant a folder's mode switches
/// to it — waiting for the next `tick` alone races the writer's very first
/// keystroke (see `arm`'s own doc). And **must not call `tick` while a modal
/// question is on screen or while shutting down**: both can be moving a
/// document's file (a save, a reload) or ending the process out from under a
/// write this engine would otherwise start.
pub struct FolderAutoSave {
    entries: Vec<AutoSaveEntry>,
}

struct AutoSaveEntry {
    document: Weak<OpenDocument>,
    /// The path this was enrolled under. Kept apart from asking the document
    /// for its path again, because a document whose file has gone missing
    /// cannot be re-resolved through [`workspace::Registry::save_mode_for`]
    /// (it canonicalizes the path, which needs the file to exist) — this is
    /// what tells `tick` "this used to be, and still might be, the same
    /// enrolled file" during an outage rather than mistaking it for a policy
    /// change.
    path: PathBuf,
    /// Only an edit whose `changed_at` is *later* than this is due to be
    /// written. Set to `now` whenever a document is newly enrolled, and
    /// advanced to the attempted text's own `changed_at` on every write
    /// attempt — success or failure — so a failing save is never retried
    /// against the same unchanged text on every tick (要件: 直らない失敗を
    /// 毎回繰り返し試みない; also covers 新規有効化時に既存の未保存内容を
    /// 無断上書きしない, since arming starts here too).
    armed_at: Instant,
    /// The `changed_at` of the text already backed up to the recovery copy
    /// while blocked. `None` once clean, so a document sitting still while
    /// blocked is not written to the recovery copy again every tick.
    protected_at: Option<Instant>,
    paused: Option<AutoSavePause>,
}

/// Why a document under `SaveMode::AutoSave` is not being written straight to
/// its file right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoSavePause {
    /// The file changed underneath, went missing, or its stamp could not be
    /// read — [`ExternalChange`] folds every read failure into `Missing`, so
    /// treating anything but `None` as blocking is already failing closed on
    /// an unreadable stamp, not only a confirmed outside edit.
    ///
    /// **Clears only when `external_change` itself reports `None` again** —
    /// a manual save or an explicit reload moves the agreed stamp — never by
    /// elapsed time, since this is re-read from scratch on every tick rather
    /// than latched.
    Conflict,
    /// Either the on-disk file carries the readonly attribute, or the
    /// document itself is a read-only comparison snapshot.
    ReadOnly,
    /// The last direct write attempt failed. `write_document_in` has already
    /// told the writer why (its own status-bar line and diagnostic), so
    /// nothing is duplicated here — this only says that a retry is what
    /// happens next, on the next edit or tick.
    WriteFailed,
}

/// What [`FolderAutoSave::status`] answers for one document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoSaveStatus {
    /// Not enrolled: its file is not under an `AutoSave` folder right now
    /// (or it has no file yet — an 無題 document can never be, since a
    /// folder's mode is decided from a path).
    Off,
    /// Enrolled, and no edit *since arming* is waiting — either nothing has
    /// been typed at all, or what is dirty predates arming and needs one
    /// fresh edit before anything is written (要件: never overwrite
    /// pre-existing dirty text just because the policy turned on).
    Idle,
    /// Enrolled, edited since arming, waiting for the debounce in
    /// [`work_copy_due`] to say it is time.
    Pending,
    Paused(AutoSavePause),
}

impl FolderAutoSave {
    /// Observe a newly visible document without rearming existing edits.
    pub fn observe(
        &mut self,
        document: &Rc<OpenDocument>,
        registry: &workspace::Registry,
        now: Instant,
    ) {
        let path = document.file.borrow().path().map(Path::to_path_buf);
        if registry.save_mode_for(path.as_deref()) != workspace::SaveMode::AutoSave {
            let outage = path
                .as_ref()
                .is_some_and(|path| path.canonicalize().is_err());
            if let Some(index) = self
                .entries
                .iter()
                .position(|entry| matches_document(entry, document))
            {
                if !outage || Some(&self.entries[index].path) != path.as_ref() {
                    self.entries.remove(index);
                    if document.text.edited() {
                        document.protective_recovery.set(true);
                        document.text.mark_pending();
                    }
                }
            }
            return;
        }
        if !self
            .entries
            .iter()
            .any(|entry| matches_document(entry, document) && Some(&entry.path) == path.as_ref())
        {
            self.arm(document, now);
        }
    }
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Arm protection for one document **immediately**, without waiting for
    /// the next [`tick`](Self::tick).
    ///
    /// Call this the instant a document opens under an `AutoSave` folder, or
    /// the instant a folder's mode switches to it. `tick` alone would only
    /// notice at its next pass — if the writer's first keystroke lands before
    /// that, `tick` would enroll the document with that keystroke already
    /// `changed_at`-stamped in the past, mistake it for text dirty before
    /// arming, and never save it (要件: 開いた直後の最初の編集を逃さない).
    /// Re-arming an already-enrolled document (a repeated call, or the
    /// document was previously blocked) resets its pause and due clock the
    /// same way — never a silent overwrite, since only edits **after** this
    /// call count from here on. A document with no file yet is a no-op.
    pub fn arm(&mut self, document: &Rc<OpenDocument>, now: Instant) {
        let Some(path) = document.file.borrow().path().map(Path::to_path_buf) else {
            return;
        };
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| matches_document(entry, document))
        {
            entry.path = path;
            entry.armed_at = now;
            entry.protected_at = None;
            entry.paused = None;
            return;
        }
        self.entries.push(AutoSaveEntry {
            document: Rc::downgrade(document),
            path,
            armed_at: now,
            protected_at: None,
            paused: None,
        });
    }

    /// Run one pass: enrol newly-eligible documents, drop ones that no
    /// longer qualify or have closed, and drive every entry that remains.
    ///
    /// `registry` is `None` when no ledger has been loaded yet — nothing can
    /// be under `AutoSave` without one, so every entry is force-backed-up and
    /// dropped rather than left to save against a stale idea of the ledger.
    ///
    /// **Must not be called while a modal question is on screen, or during
    /// shutdown** — either can be moving the same file this would write to
    /// (a save, a reload) or ending the process mid-write. Gating that is the
    /// caller's job; this only documents the requirement.
    pub fn tick(
        &mut self,
        window: &AppWindow,
        live: &Live,
        registry: Option<&workspace::Registry>,
        now: Instant,
    ) {
        let documents = open_documents(live);
        self.entries.retain(|entry| {
            documents
                .iter()
                .any(|document| matches_document(entry, document))
        });
        let Some(registry) = registry else {
            for entry in &self.entries {
                if let Some(document) = entry.document.upgrade() {
                    force_work_copy_of(window, live, &document);
                }
            }
            self.entries.clear();
            return;
        };
        for document in documents {
            let Some(path) = document.file.borrow().path().map(Path::to_path_buf) else {
                continue;
            };
            let index = self
                .entries
                .iter()
                .position(|entry| matches_document(entry, &document));
            let wanted = match path.canonicalize() {
                Ok(_) => registry.save_mode_for(Some(&path)) == workspace::SaveMode::AutoSave,
                // Unresolvable right now (missing, unmounted, ...):
                // `save_mode_for` cannot tell this apart from an explicit
                // `Recovery` folder, so never *newly* enroll here — but an
                // entry already enrolled at this same path keeps its
                // protection through the outage rather than being evicted
                // (要件: 原本が見えない間も退避は続ける).
                Err(_) => index.is_some_and(|i| self.entries[i].path == path),
            };
            match (wanted, index) {
                (true, None) => self.entries.push(AutoSaveEntry {
                    document: Rc::downgrade(&document),
                    path,
                    armed_at: now,
                    protected_at: None,
                    paused: None,
                }),
                (false, Some(i)) => {
                    // Policy moved this document out (folder demoted, or
                    // 名前を付けて保存 to somewhere else) — back up whatever
                    // is still unwritten before letting go of it.
                    force_work_copy_of(window, live, &document);
                    self.entries.remove(i);
                }
                (true, Some(i)) if self.entries[i].path != path => {
                    self.entries[i].path = path;
                    self.entries[i].armed_at = now;
                    self.entries[i].paused = None;
                    self.entries[i].protected_at = None;
                }
                _ => {}
            }
        }
        self.entries
            .retain(|entry| entry.document.upgrade().is_some());
        for entry in &mut self.entries {
            drive_entry(entry, window, live, now);
        }
    }

    /// The current mode/paused reason for one document, for the UI to show.
    pub fn status(&self, document: &Rc<OpenDocument>) -> AutoSaveStatus {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| matches_document(entry, document))
        else {
            return AutoSaveStatus::Off;
        };
        if let Some(pause) = &entry.paused {
            return AutoSaveStatus::Paused(pause.clone());
        }
        if document.text.edited() && document.text.changed_at() > entry.armed_at {
            AutoSaveStatus::Pending
        } else {
            AutoSaveStatus::Idle
        }
    }

    /// Called right before exit: **keeps 要件 8.1's recovery copy current for
    /// every enrolled document**, whatever it is currently doing, then waits
    /// to hear whether the writes landed.
    ///
    /// This is the "final flush" 要件 8.1 already has
    /// ([`flush_work_copies`]) — this covers the documents this engine is
    /// specifically protecting, on top of whatever the ordinary flush call
    /// already reaches, and works even with 要件 8.1's own switch off (the
    /// writer opted a folder into stronger protection, not a weaker one).
    /// Runs even for a document [`tick`](Self::tick) never got to — one
    /// [`arm`](Self::arm)ed right before exit is still covered, since this
    /// walks `self.entries` directly rather than only what a past `tick`
    /// already drove. Returns the number of copies that did not land, the
    /// same count [`flush_work_copies`] returns.
    pub fn force_flush(&self, window: &AppWindow, live: &Live) -> usize {
        let documents = open_documents(live);
        for entry in &self.entries {
            if !documents
                .iter()
                .any(|document| matches_document(entry, document))
            {
                continue;
            }
            if let Some(document) = entry.document.upgrade() {
                force_work_copy_of(window, live, &document);
            }
        }
        let landed = live.writer.settle(WORK_COPY_SETTLE);
        report_write_results(window, live, landed);
        documents
            .iter()
            .filter(|document| document.protective_recovery.get() && document.recovery_failed.get())
            .count()
    }
}

impl Default for FolderAutoSave {
    fn default() -> Self {
        Self::new()
    }
}

fn matches_document(entry: &AutoSaveEntry, document: &Rc<OpenDocument>) -> bool {
    entry
        .document
        .upgrade()
        .is_some_and(|held| Rc::ptr_eq(&held, document))
}

/// Whether the file at `path` currently carries the OS readonly attribute.
///
/// A metadata read that fails is not treated as readonly here — a missing or
/// unreadable file is already caught by `external_change` (folded into
/// `ExternalChange::Missing`), which is what puts the entry into
/// [`AutoSavePause::Conflict`] instead.
fn disk_read_only(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.permissions().readonly())
        .unwrap_or(false)
}

/// What, if anything, keeps this document from being written straight to its
/// file right now — re-derived from scratch every call, never latched, so a
/// conflict clears itself the moment `external_change` does (a manual save or
/// reload), never merely because time passed.
fn blocking_pause(document: &Rc<OpenDocument>, path: &Path) -> Option<AutoSavePause> {
    if document.read_only() || disk_read_only(path) {
        return Some(AutoSavePause::ReadOnly);
    }
    if document.file.borrow().external_change() != ExternalChange::None {
        return Some(AutoSavePause::Conflict);
    }
    None
}

/// Back up the current text while blocked, but only once per edit.
///
/// `force_work_copy_of` already no-ops once the shared 8.1 pending flag is
/// clear (someone else already backed up this exact text); `protected_at`
/// additionally stops *this* engine calling it again for text it has already
/// asked to be backed up, so a document sitting still while blocked is not
/// written to the recovery copy on every tick.
fn protect_while_blocked(
    window: &AppWindow,
    live: &Live,
    entry: &mut AutoSaveEntry,
    document: &Rc<OpenDocument>,
) {
    if !document.text.edited() {
        entry.protected_at = None;
        return;
    }
    let changed_at = document.text.changed_at();
    if entry.protected_at == Some(changed_at) && !document.recovery_failed.get() {
        return;
    }
    if force_work_copy_of(window, live, document) {
        entry.protected_at = Some(changed_at);
    }
}

/// Tell the writer once that a document has just become blocked, or that the
/// reason changed — never again while the same reason holds, since
/// `blocking_pause` is re-derived (and this called) on every tick.
fn notify_new_pause(window: &AppWindow, live: &Live, entry: &AutoSaveEntry, pause: &AutoSavePause) {
    if entry.paused.as_ref() == Some(pause) {
        return;
    }
    let name = entry.path.display();
    let told = match pause {
        AutoSavePause::Conflict => say!(
            "「{}」は外部で変更されたため、直接の保存を止めています。保存または読み直しで再開します",
            "\"{}\" changed outside, so direct saving is paused. Save or reload to resume",
            name
        ),
        AutoSavePause::ReadOnly => say!(
            "「{}」は読み取り専用のため、直接の保存を止めています",
            "\"{}\" is read-only, so direct saving is paused",
            name
        ),
        // `write_document_in` has already told the writer why (its own
        // status-bar line and diagnostic) at the moment it failed.
        AutoSavePause::WriteFailed => return,
    };
    window.tell_tab(told.into());
    live.cache.borrow_mut().log_diag(
        "autosave",
        &format!("paused reason={pause:?} path={}", entry.path.display()),
    );
}

fn drive_entry(entry: &mut AutoSaveEntry, window: &AppWindow, live: &Live, now: Instant) {
    let Some(document) = entry.document.upgrade() else {
        return;
    };
    if document.file.borrow().path() != Some(entry.path.as_path()) {
        return;
    }
    let pause = if crate::has_reading_view(window, live, &document) {
        Some(AutoSavePause::ReadOnly)
    } else {
        blocking_pause(&document, &entry.path)
    };
    if let Some(pause) = pause {
        notify_new_pause(window, live, entry, &pause);
        entry.paused = Some(pause);
        protect_while_blocked(window, live, entry, &document);
        return;
    }
    // Not conflicted or read-only right now. `WriteFailed` is left alone
    // here — it only clears once cleaned up below or a new attempt is made,
    // never merely because the file stopped conflicting.
    if matches!(
        entry.paused,
        Some(AutoSavePause::Conflict | AutoSavePause::ReadOnly)
    ) {
        entry.paused = None;
    }
    if !document.text.edited() {
        entry.paused = None;
        entry.protected_at = None;
        return;
    }
    let changed_at = document.text.changed_at();
    if changed_at <= entry.armed_at {
        if entry.paused == Some(AutoSavePause::WriteFailed) {
            protect_while_blocked(window, live, entry, &document);
        }
        // Nothing new since the last arm or attempt — covers both text dirty
        // from before arming and a failed write against unchanged text
        // (要件: 直らない失敗を毎回繰り返し試みない).
        return;
    }
    // AutoSave's own contract is a plain idle debounce. It does not inherit
    // 8.1's "at most 5s of continuous typing" escape hatch — that exists only
    // to bound the recovery copy's own staleness, not this engine's.
    if now.duration_since(changed_at) < WORK_COPY_IDLE {
        return;
    }
    // This text is being attempted now, pass or fail — advancing the arm
    // point here, not only on success, is what stops a failing save from
    // being retried against the same unchanged text on every tick.
    entry.armed_at = changed_at;
    let form = document.file.borrow().form();
    if write_document_in(window, live, &document, entry.path.clone(), form) {
        entry.paused = None;
        entry.protected_at = None;
    } else {
        entry.paused = Some(AutoSavePause::WriteFailed);
        protect_while_blocked(window, live, entry, &document);
    }
}

#[cfg(test)]
mod folder_auto_save_tests {
    use super::*;
    use slint::platform::software_renderer::MinimalSoftwareWindow;
    use slint::{ModelRc, SharedString, VecModel};
    use std::cell::RefCell;

    struct Offscreen(Rc<MinimalSoftwareWindow>);

    impl slint::platform::Platform for Offscreen {
        fn create_window_adapter(
            &self,
        ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    /// A minimal, offscreen `AppWindow`/`Live` pair, the same shape the other
    /// `*_ui_tests` files build — just enough to hold one document in one
    /// pane so [`open_documents`] and [`write_document_in`] work normally.
    struct Harness {
        window: AppWindow,
        live: Live,
    }

    impl Harness {
        /// Builds the offscreen window and platform first — `make_document`
        /// is only handed the window's `Weak` afterwards, because
        /// `AppWindow::new` needs a platform already set, and the document a
        /// test wants has to be built with this window's own handle.
        fn new(
            make_document: impl FnOnce(slint::Weak<AppWindow>) -> Rc<OpenDocument>,
        ) -> (Self, Rc<OpenDocument>) {
            let directory = std::env::temp_dir().join(format!(
                "editor-folder-autosave-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = Some(directory.clone()));
            let surface = MinimalSoftwareWindow::new(Default::default());
            slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
            let window = AppWindow::new().unwrap();
            let numbers = Rc::new(VecModel::from(vec![0; 2 * crate::SHEET_NUMBERS]));
            let palette = Rc::new(VecModel::from(vec![
                slint::Color::default();
                2 * crate::SHEET_COLOURS
            ]));
            let fonts = Rc::new(VecModel::from(vec![
                SharedString::default();
                2 * crate::SHEET_FONTS
            ]));
            crate::reset_settings(&numbers, &palette, &fonts);
            window.set_sheet_stride(crate::SHEET_NUMBERS as i32);
            window.set_sheet_numbers(ModelRc::from(numbers));
            window.set_palette(ModelRc::from(palette));
            window.set_sheet_fonts(ModelRc::from(fonts));
            surface.set_size(slint::PhysicalSize::new(1000, 740));
            crate::publish_panes(&window, 1);
            let id = PaneId::from_index(0);
            id.update_screen(&window, |screen| {
                screen.width = 950.0;
                screen.height = 620.0;
            });
            let document = make_document(window.as_weak());
            let tab = crate::PaneTab::showing(&window, id, document.clone());
            let live = Live {
                preview: Rc::default(),
                closed_tabs: Rc::default(),
                states: crate::PaneStates::new(&document),
                folder: Rc::default(),
                tree_paths: Rc::default(),
                workspace_ids: Rc::default(),
                results: Rc::default(),
                recent: Rc::default(),
                recent_folders: Rc::default(),
                find_terms: Rc::new(RefCell::new(crate::find::Terms::restored(Vec::new()))),
                replace_terms: Rc::new(RefCell::new(crate::find::Terms::restored(Vec::new()))),
                layout: Rc::new(RefCell::new(crate::pane_layout::Layout::single(0))),
                pending: Rc::default(),
                close_run: Rc::default(),
                cache: Rc::new(RefCell::new(crate::RenderCache::default())),
                tabs: Rc::new(RefCell::new(crate::Tabs {
                    panes: vec![crate::PaneTabs {
                        history: vec![crate::NavigationPlace::from(&tab)],
                        tabs: vec![tab],
                        ..Default::default()
                    }],
                })),
                writer: Rc::new(writer::FileWriter::start()),
                searcher: Rc::new(crate::searcher::Searcher::start(|| {})),
                searched: Rc::default(),
            };
            (Self { window, live }, document)
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.live.writer.finish();
            app_data::TEST_DIRECTORY.with(|held| *held.borrow_mut() = None);
        }
    }

    /// A change recorded through undo, the way real typing is — plain
    /// mutation of `text` would leave `changed_at`/`pending_since` untouched.
    fn edit(document: &Rc<OpenDocument>, text: &str) {
        let at = document.text.borrow().len();
        document.history.borrow_mut().separate_next = true;
        document.record(at, String::new(), text.into());
        document.text.borrow_mut().push_str(text);
    }

    fn open_under(
        directory: &Path,
        name: &str,
        text: &str,
        window: slint::Weak<AppWindow>,
    ) -> Rc<OpenDocument> {
        let path = directory.join(name);
        std::fs::write(&path, text).unwrap();
        let (file, loaded) = DocumentFile::open(&path, MAX_DOCUMENT_CHARACTERS).unwrap();
        OpenDocument::new(file, loaded, window)
    }

    fn autosave_registry(root: &Path) -> workspace::Registry {
        let mut registry = workspace::Registry::new();
        let workspace_id = registry
            .create_workspace("試験".to_owned())
            .expect("creates");
        let folder = registry.add_root(workspace_id, root).expect("registers");
        registry
            .set_folder_mode(folder, workspace::SaveMode::AutoSave)
            .expect("sets");
        registry
    }

    fn scratch_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "editor-folder-autosave-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn preexisting_dirty_text_is_never_overwritten_by_arming_alone() {
        let directory = std::env::temp_dir().join(format!(
            "editor-folder-autosave-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let registry = autosave_registry(&directory);
        let doc_directory = directory.clone();
        let (harness, document) =
            Harness::new(|weak| open_under(&doc_directory, "原稿.md", "元の本文", weak));
        edit(&document, "未保存の追記");
        assert!(document.text.edited());

        let mut engine = FolderAutoSave::new();
        let armed_at = Instant::now();
        engine.tick(&harness.window, &harness.live, Some(&registry), armed_at);

        assert_eq!(
            std::fs::read_to_string(directory.join("原稿.md")).unwrap(),
            "元の本文",
            "arming must not silently write dirty text that predates it"
        );
        assert_eq!(engine.status(&document), AutoSaveStatus::Idle);

        // A fresh edit after arming is what makes it due.
        edit(&document, "、続き");
        let due_at = Instant::now() + WORK_COPY_LONGEST + Duration::from_millis(1);
        engine.tick(&harness.window, &harness.live, Some(&registry), due_at);

        assert_eq!(
            std::fs::read_to_string(directory.join("原稿.md")).unwrap(),
            "元の本文未保存の追記、続き"
        );
        assert!(!document.text.edited());
        assert_eq!(engine.status(&document), AutoSaveStatus::Idle);
    }

    #[test]
    fn a_newly_opened_clean_document_saves_on_its_first_edit() {
        let directory = std::env::temp_dir().join(format!(
            "editor-folder-autosave-clean-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let registry = autosave_registry(&directory);
        let doc_directory = directory.clone();
        let (harness, document) =
            Harness::new(|weak| open_under(&doc_directory, "きれい.md", "本文", weak));
        assert!(!document.text.edited());

        let mut engine = FolderAutoSave::new();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );
        assert_eq!(engine.status(&document), AutoSaveStatus::Idle);

        edit(&document, "追記");
        let due_at = Instant::now() + WORK_COPY_LONGEST + Duration::from_millis(1);
        engine.tick(&harness.window, &harness.live, Some(&registry), due_at);

        assert_eq!(
            std::fs::read_to_string(directory.join("きれい.md")).unwrap(),
            "本文追記"
        );
        assert!(!document.text.edited());
    }

    #[test]
    fn a_conflict_pauses_the_direct_write_and_still_updates_the_recovery_copy() {
        let directory = std::env::temp_dir().join(format!(
            "editor-folder-autosave-conflict-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let registry = autosave_registry(&directory);
        let doc_directory = directory.clone();
        let (harness, document) =
            Harness::new(|weak| open_under(&doc_directory, "競合.md", "本文", weak));
        // 要件 8.1 の自動退避そのものは切ってある——それでも、フォルダの
        // AutoSave 方式が塞がれている間は最新の内容を退避で守る。
        harness.window.set_autosave(false);

        let mut engine = FolderAutoSave::new();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );

        // Another program changes the file underneath.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(directory.join("競合.md"), "外部の変更").unwrap();

        edit(&document, "書き手の追記");
        let due_at = Instant::now() + WORK_COPY_LONGEST + Duration::from_millis(1);
        engine.tick(&harness.window, &harness.live, Some(&registry), due_at);

        assert_eq!(
            std::fs::read_to_string(directory.join("競合.md")).unwrap(),
            "外部の変更",
            "a conflicted document must not be overwritten"
        );
        assert_eq!(
            engine.status(&document),
            AutoSaveStatus::Paused(AutoSavePause::Conflict)
        );
        assert!(
            document.text.edited(),
            "the unsaved edit is kept, not discarded"
        );

        // The recovery copy still got the latest text, bypassing 8.1's own
        // switch, and it survives a forced flush.
        harness.live.writer.settle(WORK_COPY_SETTLE);
        let work_directory = app_data::work_directory().unwrap();
        let copies = app_data::read_all_in(&work_directory);
        let copy = copies
            .iter()
            .find(|copy| copy.origin.as_deref() == Some(directory.join("競合.md").as_path()))
            .expect("a recovery copy exists although 8.1's switch is off");
        assert_eq!(copy.text, "本文書き手の追記");

        let lost = engine.force_flush(&harness.window, &harness.live);
        assert_eq!(lost, 0);
    }

    #[test]
    fn a_missing_original_keeps_protection_without_dropping_the_entry() {
        let directory = scratch_directory("missing");
        let registry = autosave_registry(&directory);
        let doc_directory = directory.clone();
        let (harness, document) =
            Harness::new(|weak| open_under(&doc_directory, "消える.md", "本文", weak));
        harness.window.set_autosave(false);

        let mut engine = FolderAutoSave::new();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );

        std::fs::remove_file(directory.join("消える.md")).unwrap();
        edit(&document, "本体が消えている間の追記");
        let due_at = Instant::now() + WORK_COPY_LONGEST;
        engine.tick(&harness.window, &harness.live, Some(&registry), due_at);

        // Still enrolled and blocked — `save_mode_for` cannot resolve the
        // missing path, but that must not be mistaken for the policy having
        // turned off.
        assert_eq!(
            engine.status(&document),
            AutoSaveStatus::Paused(AutoSavePause::Conflict)
        );
        assert!(!directory.join("消える.md").exists());

        harness.live.writer.settle(WORK_COPY_SETTLE);
        let copies = app_data::read_all_in(&app_data::work_directory().unwrap());
        let copy = copies
            .iter()
            .find(|copy| copy.origin.as_deref() == Some(directory.join("消える.md").as_path()))
            .expect("the recovery copy is kept through the outage");
        assert_eq!(copy.text, "本文本体が消えている間の追記");
    }

    #[test]
    fn demoting_the_folder_backs_up_before_ending_eligibility() {
        let directory = scratch_directory("demote");
        let mut registry = autosave_registry(&directory);
        let doc_directory = directory.clone();
        let (harness, document) =
            Harness::new(|weak| open_under(&doc_directory, "降格.md", "本文", weak));
        harness.window.set_autosave(false);

        let mut engine = FolderAutoSave::new();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );
        edit(&document, "追記");

        // Switched back to Recovery before the idle debounce would have
        // fired the direct write.
        let folder = registry.folders()[0].id;
        registry
            .set_folder_mode(folder, workspace::SaveMode::Recovery)
            .unwrap();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );

        assert_eq!(engine.status(&document), AutoSaveStatus::Off);
        assert_eq!(
            std::fs::read_to_string(directory.join("降格.md")).unwrap(),
            "本文",
            "must never write straight to the file it is leaving"
        );

        harness.live.writer.settle(WORK_COPY_SETTLE);
        let copies = app_data::read_all_in(&app_data::work_directory().unwrap());
        let copy = copies
            .iter()
            .find(|copy| copy.origin.as_deref() == Some(directory.join("降格.md").as_path()))
            .expect("outstanding edits are backed up before eligibility ends");
        assert_eq!(copy.text, "本文追記");
    }

    #[test]
    fn a_failed_write_is_not_retried_until_a_new_edit_arrives() {
        let directory = scratch_directory("write-failed");
        let registry = autosave_registry(&directory);
        let doc_directory = directory.clone();
        let (harness, document) =
            Harness::new(|weak| open_under(&doc_directory, "失敗.md", "本文", weak));
        // Fixes the document's own written form to Shift_JIS, so a character
        // it cannot represent fails the direct write deterministically and
        // portably — no filesystem permission bits involved.
        document
            .file
            .borrow_mut()
            .save_to_as(
                directory.join("失敗.md"),
                "本文",
                file_io::TextForm {
                    encoding: Encoding::Cp932,
                    ..file_io::TextForm::default()
                },
            )
            .unwrap();

        let mut engine = FolderAutoSave::new();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );

        edit(&document, "😀"); // unrepresentable in Shift_JIS
        let due_at = Instant::now() + WORK_COPY_LONGEST;
        engine.tick(&harness.window, &harness.live, Some(&registry), due_at);
        assert_eq!(
            engine.status(&document),
            AutoSaveStatus::Paused(AutoSavePause::WriteFailed)
        );

        // Same unchanged failing text: ticking again must not retry.
        harness.window.set_render_status(SharedString::default());
        let still_due_at = due_at + Duration::from_secs(1);
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            still_due_at,
        );
        assert!(
            harness.window.get_render_status().is_empty(),
            "an unchanged failing edit must not be retried every tick"
        );

        // A genuinely new edit gets a fresh attempt.
        edit(&document, "、続き");
        let retry_at = still_due_at + WORK_COPY_LONGEST;
        engine.tick(&harness.window, &harness.live, Some(&registry), retry_at);
        assert!(
            !harness.window.get_render_status().is_empty(),
            "a new edit is attempted again"
        );
    }

    #[test]
    fn arming_at_open_time_catches_an_edit_made_before_the_first_tick() {
        let directory = scratch_directory("arm-early");
        let registry = autosave_registry(&directory);
        let doc_directory = directory.clone();
        let (harness, document) =
            Harness::new(|weak| open_under(&doc_directory, "先取り.md", "本文", weak));

        // Armed the instant the document opens, before any tick — the way
        // integration must call this for a document opened straight into an
        // `AutoSave` folder.
        let mut engine = FolderAutoSave::new();
        engine.arm(&document, Instant::now());
        edit(&document, "最初の編集");

        let due_at = Instant::now() + WORK_COPY_LONGEST;
        engine.tick(&harness.window, &harness.live, Some(&registry), due_at);

        assert_eq!(
            std::fs::read_to_string(directory.join("先取り.md")).unwrap(),
            "本文最初の編集",
            "the very first edit after opening must not be treated as pre-existing"
        );
    }

    #[test]
    fn a_backup_already_written_by_something_else_is_not_written_again() {
        let directory = scratch_directory("already-backed-up");
        let registry = autosave_registry(&directory);
        let doc_directory = directory.clone();
        let (harness, document) =
            Harness::new(|weak| open_under(&doc_directory, "既済.md", "本文", weak));
        edit(&document, "普通の退避で先に書かれる分");
        // 要件 8.1 の通常の退避が先に走った体で、共有の pending 旗を下ろす。
        write_work_copy_of(&harness.window, &harness.live, &document);
        harness.live.writer.settle(WORK_COPY_SETTLE);

        std::fs::write(directory.join("既済.md"), "外部の変更").unwrap();
        let mut engine = FolderAutoSave::new();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );
        assert_eq!(
            engine.status(&document),
            AutoSaveStatus::Paused(AutoSavePause::Conflict)
        );

        let before = app_data::read_all_in(&app_data::work_directory().unwrap());
        let landed_before = before
            .iter()
            .find(|copy| copy.origin.as_deref() == Some(directory.join("既済.md").as_path()))
            .expect("the ordinary recovery copy already covers this text");
        assert_eq!(landed_before.text, "本文普通の退避で先に書かれる分");

        // Ticking again without a new edit must not write a second time.
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );
        harness.live.writer.settle(WORK_COPY_SETTLE);
        let after = app_data::read_all_in(&app_data::work_directory().unwrap());
        assert_eq!(before, after);
    }

    #[test]
    fn protective_copy_survives_backup_off_and_restores_a_missing_original() {
        let root = scratch_directory("protected-restart");
        let (harness, document) =
            Harness::new(|weak| open_under(&root, "draft.md", "original", weak));
        let stamp = document.file.borrow().agreed_stamp();
        edit(&document, " unsaved");
        // An ordinary copy clearing the pending flag must still be promoted.
        write_work_copy_of(&harness.window, &harness.live, &document);
        harness.live.writer.settle(WORK_COPY_SETTLE);
        document.text.work_copy_written();
        harness.window.set_autosave(false);
        app_data::write_settings(
            &app_data::app_directory().unwrap(),
            &[(AUTOSAVE_SETTING.into(), "0".into())],
        )
        .unwrap();
        std::fs::remove_file(root.join("draft.md")).unwrap();
        assert!(force_work_copy_of(
            &harness.window,
            &harness.live,
            &document
        ));
        // Includes queued protection, before it has necessarily landed.
        assert_eq!(discard_all_work_copies(&harness.live), 0);
        harness.live.writer.settle(WORK_COPY_SETTLE);
        let records = app_data::read_records_in(&app_data::work_directory().unwrap());
        assert_eq!(records.len(), 1);
        assert!(records[0].1);
        assert_eq!(records[0].0.stamp, stamp);
        let restored = restore_tabs(&harness.window);
        assert_eq!(restored.len(), 1);
        assert_eq!(&*restored[0].0.text.borrow(), "original unsaved");
        assert!(restored[0].0.protective_recovery.get());
        assert!(restored[0].0.missing.get());
        edit(&document, " later");
        write_work_copy_of(&harness.window, &harness.live, &document);
        harness.live.writer.settle(WORK_COPY_SETTLE);
        assert!(app_data::read_records_in(&app_data::work_directory().unwrap())[0].1);
    }

    #[test]
    fn backup_off_does_not_restore_ordinary_body_that_mentions_protection() {
        let root = scratch_directory("ordinary-restart");
        let (harness, document) =
            Harness::new(|weak| open_under(&root, "draft.md", "original", weak));
        edit(&document, "\n\nprotected: 1\n");
        write_work_copy_of(&harness.window, &harness.live, &document);
        harness.live.writer.settle(WORK_COPY_SETTLE);
        app_data::write_settings(
            &app_data::app_directory().unwrap(),
            &[(AUTOSAVE_SETTING.into(), "0".into())],
        )
        .unwrap();
        assert!(restore_tabs(&harness.window).is_empty());
        assert!(!app_data::read_records_in(&app_data::work_directory().unwrap())[0].1);
    }

    #[test]
    fn changing_target_never_writes_the_previous_path() {
        let root = scratch_directory("save-as-path");
        let registry = autosave_registry(&root);
        let (harness, document) =
            Harness::new(|weak| open_under(&root, "old.md", "original", weak));
        let mut engine = FolderAutoSave::new();
        engine.arm(&document, Instant::now());
        let form = document.file.borrow().form();
        document
            .file
            .borrow_mut()
            .save_to_as(root.join("new.md"), "original", form)
            .unwrap();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now(),
        );
        edit(&document, " changed");
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now() + WORK_COPY_LONGEST,
        );
        assert_eq!(
            std::fs::read_to_string(root.join("old.md")).unwrap(),
            "original"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("new.md")).unwrap(),
            "original changed"
        );
    }

    #[test]
    fn closed_document_kept_for_reopen_is_not_saved() {
        let root = scratch_directory("closed-retained");
        let registry = autosave_registry(&root);
        let (harness, document) =
            Harness::new(|weak| open_under(&root, "draft.md", "original", weak));
        let mut engine = FolderAutoSave::new();
        engine.arm(&document, Instant::now());
        edit(&document, " changed");
        harness.live.tabs.borrow_mut().panes[0].tabs.clear();
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now() + WORK_COPY_LONGEST,
        );
        assert_eq!(
            std::fs::read_to_string(root.join("draft.md")).unwrap(),
            "original"
        );
        assert_eq!(engine.status(&document), AutoSaveStatus::Off);
    }

    #[test]
    fn failed_protective_copy_retries_unchanged_text_and_successful_save_retires_it() {
        let root = scratch_directory("protected-retry");
        let (harness, document) =
            Harness::new(|weak| open_under(&root, "draft.md", "original", weak));
        harness.window.set_autosave(false);
        edit(&document, " unsaved");
        let directory = app_data::work_directory().unwrap();
        std::fs::create_dir_all(directory.parent().unwrap()).unwrap();
        std::fs::write(&directory, b"not a directory").unwrap();
        assert!(force_work_copy_of(
            &harness.window,
            &harness.live,
            &document
        ));
        let reports = harness.live.writer.settle(WORK_COPY_SETTLE);
        assert_eq!(
            report_write_results(&harness.window, &harness.live, reports),
            1
        );
        assert!(document.recovery_failed.get());
        assert!(document.text.pending_since().is_some());
        std::fs::remove_file(&directory).unwrap();
        assert!(force_work_copy_of(
            &harness.window,
            &harness.live,
            &document
        ));
        let reports = harness.live.writer.settle(WORK_COPY_SETTLE);
        assert_eq!(
            report_write_results(&harness.window, &harness.live, reports),
            0
        );
        assert!(!document.recovery_failed.get());
        assert_eq!(
            app_data::read_records_in(&directory)[0].0.text,
            "original unsaved"
        );
        let form = document.file.borrow().form();
        assert!(write_document_in(
            &harness.window,
            &harness.live,
            &document,
            root.join("draft.md"),
            form
        ));
        harness.live.writer.settle(WORK_COPY_SETTLE);
        assert!(!document.protective_recovery.get());
        assert!(app_data::read_records_in(&directory).is_empty());
    }

    #[test]
    fn disabling_and_reenabling_before_a_tick_rearms_existing_dirty_text() {
        let root = scratch_directory("rapid-policy-toggle");
        let mut registry = autosave_registry(&root);
        let folder = registry.folders().first().unwrap().id;
        let (harness, document) =
            Harness::new(|weak| open_under(&root, "draft.md", "original", weak));
        let mut engine = FolderAutoSave::new();
        engine.observe(&document, &registry, Instant::now());
        edit(&document, " existing");
        registry
            .set_folder_mode(folder, workspace::SaveMode::Recovery)
            .unwrap();
        engine.observe(&document, &registry, Instant::now());
        assert_eq!(engine.status(&document), AutoSaveStatus::Off);
        assert!(document.protective_recovery.get());
        registry
            .set_folder_mode(folder, workspace::SaveMode::AutoSave)
            .unwrap();
        engine.observe(&document, &registry, Instant::now());
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now() + WORK_COPY_LONGEST,
        );
        assert_eq!(
            std::fs::read_to_string(root.join("draft.md")).unwrap(),
            "original"
        );
        edit(&document, " new");
        engine.tick(
            &harness.window,
            &harness.live,
            Some(&registry),
            Instant::now() + WORK_COPY_LONGEST,
        );
        assert_eq!(
            std::fs::read_to_string(root.join("draft.md")).unwrap(),
            "original existing new"
        );
    }
}
