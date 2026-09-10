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
//! - **8.3 外部変更。**未編集なら黙って読み直し、編集中なら知らせるだけ。
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
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use crate::buffer::{DocumentFile, ExternalChange};
use crate::file_io::{self, Encoding, LoadError};
use crate::open_document::OpenDocument;
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
    let Some(directory) = app_data::work_directory() else {
        return;
    };
    let path = directory.join(app_data::work_file_name(copy));
    if live.writer.remove(path) {
        return;
    }
    let _ = app_data::discard_in(&directory, copy);
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
    let _ = &directory;
    let mut named = Vec::new();
    let mut copies = Vec::new();
    for copy in app_data::read_all_in(&directory).into_iter().chain(
        open_documents(live)
            .iter()
            .map(|document| work_identity(&document.file.borrow())),
    ) {
        let name = app_data::work_file_name(&copy);
        if named.contains(&name) {
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
/// The pending run is cleared whether the write succeeded or not. Left set, a
/// failing write would be retried at every tick for as long as the editor ran;
/// cleared, the next keystroke asks again, which is the same answer arrived at
/// without filling the log.
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
    if !window.get_autosave() {
        return;
    }
    if document.text.pending_since().is_none() {
        return;
    }
    let cache = &live.cache;
    let file = &document.file;
    let Some(directory) = app_data::work_directory() else {
        return;
    };
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
    let bytes = app_data::encode(&copy).into_bytes();
    let length = bytes.len();
    if live.writer.write(path.clone(), bytes) {
        cache
            .borrow_mut()
            .log_diag("work", &format!("queued bytes={length}"));
        return;
    }
    // No writer thread. Written here instead, which is what this did before
    // the thread existed.
    let started = Instant::now();
    let outcome = app_data::write_into(&directory, &copy);
    let elapsed = elapsed_ms(started);
    let message = match outcome {
        Ok(path) => {
            let shown = path.display();
            format!("saved bytes={length} ms={elapsed:.2} path={shown}")
        }
        Err(error) => format!("failed bytes={length} error={error}"),
    };
    cache.borrow_mut().log_diag("work", &message);
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
        // `mark_pending`は時計を今から数え直すので、**次の試みは2秒後**に
        // なる：同じ失敗を毎秒繰り返すのではなく、間を置いて一度。
        //
        // **新しい編集が来ていれば何もしない**（`mark_pending`は待っている
        // 旗があれば触らない）。そちらの時計のほうが正しい。
        let named = result.path.file_name().and_then(|name| name.to_str());
        let failed = open_documents(live).into_iter().find(|document| {
            let copy = work_identity(&document.file.borrow());
            named == Some(app_data::work_file_name(&copy).as_str())
        });
        if let Some(document) = failed {
            document.text.mark_pending();
        }
        // **画面にも出す。**要件 8.1 は書き手への約束なので、守れていないことは
        // 書き手が知っていなければならない。1件目だけ——同じ理由で失敗した
        // 数件が順に上書きし合っても、読めるのは最後の1つである。
        let told = if result.removed {
            "作業コピーを片づけられませんでした"
        } else {
            "作業コピーを退避できませんでした。まもなく再試行します"
        };
        window.set_render_status(told.into());
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
    let document = live.active(window);
    let file = &document.file;
    if file.borrow().external_change() != ExternalChange::Modified {
        return;
    }
    let Some(stamp) = file.borrow().current_stamp() else {
        return;
    };
    if !file.borrow_mut().take_report(stamp) {
        return;
    }
    if document.text.edited() {
        window.set_render_status("別のアプリがこのファイルを変更しました".into());
        live.cache
            .borrow_mut()
            .log_diag("external", "modified edited=1 action=notify");
        return;
    }
    reload_from_file(window, live);
}

/// Take the file as it now is, in place of what the editor holds (要件 8.3).
///
/// **Also the answer that throws work away**, when it is chosen from the
/// conflict question rather than reached with nothing unsaved. The work copy
/// held exactly what is being given up, so it goes too — left behind, it would
/// bring the discarded text back at the next start.
pub fn reload_from_file(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    let reloaded = document.file.borrow_mut().reload(MAX_DOCUMENT_CHARACTERS);
    match reloaded {
        Some(Ok(text)) => {
            let bytes = text.len();
            replace_document(window, &live.states, &live.cache, &document, text);
            // Reloaded, not edited: the text and the file agree by definition.
            document.text.mark_saved();
            discard_work_copy(live, &work_identity(&document.file.borrow()));
            publish_tabs(window, live);
            window.set_render_status("外部の変更を読み込みました".into());
            live.cache
                .borrow_mut()
                .log_diag("external", &format!("reloaded bytes={bytes}"));
        }
        Some(Err(error)) => {
            window.set_render_status(format!("読み直せません: {error}").into());
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
                format!("{name}のまま読み直しました（字は変わりません）")
            } else {
                format!("{name}で開き直しました")
            };
            let told = if mixed {
                format!("{done}（改行コードは混在していました）")
            } else {
                done
            };
            window.set_render_status(told.into());
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
                    format!("{name}としては読めません。文書はそのままです")
                }
                other => format!("開き直せません: {other}"),
            };
            window.set_render_status(told.into());
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
/// thrown away. When the original has gone, the text is kept as an untitled
/// buffer rather than lost — not losing what was typed is the point, and the
/// name is the lesser half of it.
pub fn restore_tabs(window: &AppWindow) -> Vec<(Rc<OpenDocument>, EditorState)> {
    // 追加要件 2026-09-08: 自動退避を切ってあれば、戻すものは無い。切った
    // ときに全部消しているので普段はここに何も残っていないが、**設定ファイル
    // を手で書き換えた場合は残っている**——そのときも、切ってあると言われた
    // なら戻さない。
    if !autosave_wanted() {
        return Vec::new();
    }
    let Some(directory) = app_data::work_directory() else {
        return Vec::new();
    };
    let mut tabs = Vec::new();
    for copy in app_data::read_all_in(&directory) {
        let untitled = copy.untitled.max(1);
        let file = match &copy.origin {
            Some(path) => match DocumentFile::open(path, MAX_DOCUMENT_CHARACTERS) {
                Ok((file, _)) => file,
                Err(_) => DocumentFile::untitled(untitled),
            },
            None => DocumentFile::untitled(untitled),
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
        document.text.mark_restored();
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
    // 要件 8.2: the ordinary Ctrl+S is silent, and the one thing it stops for
    // is a file that has changed underneath since it was opened. 要件 8.3 gives
    // that four answers, so the writing waits for one.
    let outside_change = file.borrow().external_change() == ExternalChange::Modified;
    if Some(&target) == existing.as_ref() && outside_change {
        let title = file.borrow().title();
        ask_question(
            window,
            live,
            Question::SaveConflict,
            format!(
                "「{title}」は別のアプリで変更されています。\n\n\
                 読み込むと、保存していない変更は失われます。"
            ),
            &[
                "作業中の内容で上書き",
                "外部の変更を読み込む",
                "別名で保存",
                "キャンセル",
            ],
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
        let told = format!(
            "{}{mark}・{}で保存しました",
            form.encoding.as_str(),
            crate::newline_name(form.newline)
        );
        window.set_render_status(told.into());
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

/// Overwrite the file with what is in the editor, outside change and all
/// (要件 8.3, the first of the four).
pub fn overwrite_the_outside_change(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    let path = document.file.borrow().path().map(Path::to_path_buf);
    let Some(path) = path else {
        return;
    };
    write_document_to(window, live, &document, path);
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
    let cache = &live.cache;
    let file = &document.file;
    let text = document.text.borrow().clone();
    let bytes = text.len();
    let shown = target.display().to_string();
    // Taken before the save, because 名前を付けて保存 moves the document to
    // another file and the copy on disk is still under the old name.
    let previous = work_identity(&file.borrow());
    let saved_to = target.clone();
    let outcome = file.borrow_mut().save_to_as(target, &text, form);
    match outcome {
        Ok(()) => {
            document.text.mark_saved();
            discard_work_copy(live, &previous);
            discard_work_copy(live, &work_identity(&file.borrow()));
            // The name in the strip changes with 名前を付けて保存, and the
            // unsaved marker changes with every save.
            publish_tabs(window, live);
            // 要件 E2: **帯は、書いた形をすぐ言う**（書き手の報告 2026-09-10）。
            // 打鍵で組み直すまで待たない——保存は本文を1字も動かさないので、
            // その組み直しは来ない。
            crate::publish_active_encoding(window, live);
            window.set_render_status("保存しました".into());
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
                    "save ok bytes={bytes} as={} path={shown}",
                    form.encoding.as_str()
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
        Err(file_io::SaveError::Unmappable(character)) => {
            // **どの字かは言わない**（書き手の判断 2026-09-10：「一文字に限らない
            // ので」）。1字だけ挙げれば、それを直せば済むように読める——実際には
            // 次の字でまた断られる。書き手が次にすることは**UTF-8で保存する**で
            // あって、字を1つずつ潰していくことではない。
            //
            // **見つけた字は診断ログに残す**（`first=`）。「なぜ保存できないのか」を
            // 後から辿る手掛かりは要る——画面に出すかどうかとは別の話である。
            window.set_render_status(
                format!(
                    "{}では表せない文字があるため保存できません。UTF-8で保存してください",
                    form.encoding.as_str()
                )
                .into(),
            );
            cache.borrow_mut().log_diag(
                "encoding",
                &format!(
                    "unmappable as={} first={character} path={shown}",
                    form.encoding.as_str()
                ),
            );
            false
        }
        Err(error) => {
            window.set_render_status(format!("保存できません: {error}").into());
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
    let mut saved = 0;
    let mut failed = 0;
    let mut conflicted = 0;
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
        if document.file.borrow().external_change() == ExternalChange::Modified {
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
    let owner = ime::window_handle(window);
    let mut left = 0;
    let mut stopped = false;
    for document in unnamed {
        if stopped {
            left += 1;
            continue;
        }
        let suggested = document.file.borrow().title();
        let held = document.file.borrow().form();
        let Some(chosen) = file_dialog::save_document_as(owner, &suggested, save_fields(held))
        else {
            stopped = true;
            left += 1;
            continue;
        };
        let form = crate::save_form_of_id(chosen.encoding, chosen.newline, held);
        if write_document_in(window, live, &document, chosen.path, form) {
            saved += 1;
        } else {
            failed += 1;
        }
    }
    // Written last, over whatever the individual saves said: the count is the
    // answer to 全て保存, and one of the writes saying 保存しました is not.
    let mut told = format!("{saved}件を保存しました");
    if failed > 0 {
        told.push_str(&format!("／{failed}件は保存できません"));
    }
    if left > 0 {
        told.push_str(&format!("／無題{left}件は保存していません"));
    }
    if conflicted > 0 {
        told.push_str(&format!("／外部変更{conflicted}件は個別に保存してください"));
    }
    window.set_render_status(told.clone().into());
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
        window.set_render_status("まだ保存していない文書です".into());
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
