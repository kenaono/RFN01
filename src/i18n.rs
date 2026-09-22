//! 表示の言語（追加要件 2026-09-15「表示の国際化対応」、書き手）。
//!
//! **画面（`.slint`）の文言は`@tr("English")`**で書き、日本語の訳は
//! `translations/ja/LC_MESSAGES/editor_spike.po`から実行ファイルへ埋め込む（`build.rs`）。
//! **Rustが画面へ渡す文言**（Keysの操作名など）は、ここの[`pick`]で日本語と英語の対から選ぶ。
//!
//! 設定は「System」「日本語」「English」。**Systemは Windows の表示言語が日本語なら日本語、
//! それ以外は英語**（対応する言語が無ければ英語、が書き手の決め）。

use std::sync::atomic::{AtomicBool, Ordering};

/// 設定ファイルの`language`。0（と知らない値）はシステムに合わせる。
pub const JAPANESE: i32 = 1;
pub const ENGLISH: i32 = 2;

/// いま日本語で出しているか。**画面の外のスレッド**（検索など）の文言も同じ答えを読むので
/// スレッドに閉じない。
///
/// **初めは日本語**：アプリは起動時に必ず[`apply`]で決め直す。決め直さないのは試験だけで、
/// 試験の期待はこの編集器がもともと出していた日本語の文言で書いてある。
static SHOWING_JAPANESE: AtomicBool = AtomicBool::new(true);

pub fn japanese() -> bool {
    #[cfg(test)]
    if let Some(japanese) = TEST_LANGUAGE.with(std::cell::Cell::get) {
        return japanese;
    }
    SHOWING_JAPANESE.load(Ordering::Relaxed)
}

// 試験は並んで走り、ほかの試験は日本語の文言を期待している。**言語を変える試験は自分の
// スレッドだけで変える。**
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_LANGUAGE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// 日本語と英語の対から、いまの言語のほうを選ぶ。
pub fn pick<'a>(japanese_text: &'a str, english_text: &'a str) -> &'a str {
    if japanese() {
        japanese_text
    } else {
        english_text
    }
}

/// 日本語と英語の対から、いまの言語のほうを`format!`で組む（国際化②）。
///
/// **書式の文字列は定数でなければならない**ので、[`pick`]で選んでから`format!`へは
/// 渡せない——選ぶのを式の外に出したのがこれ。`{title}`のような名前での取り込みは
/// どちらの文にも効き、`{}`へ渡す値は後ろに並べる（走るのは片方だけ）。
#[macro_export]
macro_rules! say {
    ($japanese:literal, $english:literal $(, $argument:expr)* $(,)?) => {
        if $crate::i18n::japanese() {
            format!($japanese $(, $argument)*)
        } else {
            format!($english $(, $argument)*)
        }
    };
}

/// Windows の表示言語が日本語か。
fn system_is_japanese() -> bool {
    // SAFETY: 引数の無い問い合わせ。
    let language = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    // 主言語は下位10ビット。LANG_JAPANESE = 0x11。
    language & 0x3ff == 0x11
}

/// 設定の選択から言語を決めて、画面の訳を切り替える。返すのは日本語で出すか。
///
/// **最初の部品（窓）を作ったあとに呼ぶ**——Slint の決まり。
pub fn apply(choice: i32) -> bool {
    let japanese = match choice {
        JAPANESE => true,
        ENGLISH => false,
        _ => system_is_japanese(),
    };
    SHOWING_JAPANESE.store(japanese, Ordering::Relaxed);
    // 英語は訳の無い元の文言なので、空の名前で選ぶ。
    let _ = slint::select_bundled_translation(if japanese { "ja" } else { "" });
    japanese
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    /// `@tr("…")`の文言が、全部日本語の訳を持っているか。**訳し漏れは画面で英語のまま
    /// 出るだけで、ビルドも試験も通ってしまう**ので、ここで数える。
    #[test]
    fn every_screen_text_has_a_japanese_translation() {
        let unescape = |text: &str| text.replace("\\\"", "\"").replace("\\n", "\n");
        let sources = [
            include_str!("../ui/app-window.slint"),
            include_str!("../ui/editor-pane.slint"),
            include_str!("../ui/editor-surface.slint"),
            include_str!("../ui/title-menu-bar.slint"),
            include_str!("../ui/controls.slint"),
            include_str!("../ui/diff-window.slint"),
            include_str!("../ui/quick-draft.slint"),
            include_str!("../ui/workspace-manager.slint"),
            include_str!("../ui/panel-style-editor.slint"),
        ];
        let mut used = HashSet::new();
        for source in sources {
            for line in source.lines() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let mut rest = line;
                while let Some(at) = rest.find("@tr(") {
                    rest = rest[at + 4..].trim_start();
                    let Some(body) = rest.strip_prefix('"') else {
                        break;
                    };
                    let mut end = 0;
                    let bytes = body.as_bytes();
                    while end < bytes.len() && bytes[end] != b'"' {
                        end += if bytes[end] == b'\\' { 2 } else { 1 };
                    }
                    used.insert(unescape(&body[..end.min(body.len())]));
                    rest = &body[end.min(body.len())..];
                }
            }
        }
        let po = include_str!("../translations/ja/LC_MESSAGES/editor_spike.po");
        let mut translated = HashSet::new();
        let mut id: Option<String> = None;
        for line in po.lines() {
            // 両端の`"`を1つずつ外す。**`trim_matches`では足りない**——`\"`で終わる文言の
            // 最後の引用符まで食べる。
            let quoted = |text: &str| {
                text.strip_prefix('"')
                    .and_then(|inner| inner.strip_suffix('"'))
                    .map(unescape)
                    .unwrap_or_default()
            };
            if let Some(text) = line.strip_prefix("msgid ") {
                id = Some(quoted(text));
            } else if let Some(text) = line.strip_prefix("msgstr ") {
                if let Some(id) = id.take()
                    && !quoted(text).is_empty()
                {
                    translated.insert(id);
                }
            }
        }
        let mut missing: Vec<_> = used.difference(&translated).collect();
        missing.sort();
        assert!(used.len() > 200, "found {} texts", used.len());
        assert!(missing.is_empty(), "no Japanese for: {missing:?}");
    }

    /// 国際化②: **Rustから画面へ出す日本語の文は、英語と対になっているか。**
    ///
    /// 日本語の字（かな・漢字）を含む文字列は、すぐ後ろに`, "英語"`が続いていなければならない
    /// ——[`pick`](super::pick)・[`say!`]・Keysの操作名の組がその形である。対の無い文は英語の画面に
    /// 日本語のまま出るだけで、ビルドも試験も通ってしまうので、ここで数える。
    /// 画面に出ないもの（記法・診断ログ・試験の本文）だけを名指しで外す。
    #[test]
    fn every_rust_message_has_an_english_pair() {
        let sources = [
            ("app_data.rs", include_str!("app_data.rs")),
            ("buffer.rs", include_str!("buffer.rs")),
            ("clipboard.rs", include_str!("clipboard.rs")),
            ("code_page.rs", include_str!("code_page.rs")),
            ("comparison.rs", include_str!("comparison.rs")),
            ("diag.rs", include_str!("diag.rs")),
            ("diff_view.rs", include_str!("diff_view.rs")),
            ("directwrite_probe.rs", include_str!("directwrite_probe.rs")),
            (
                "directwrite_render.rs",
                include_str!("directwrite_render.rs"),
            ),
            (
                "directwrite_render/incremental.rs",
                include_str!("directwrite_render/incremental.rs"),
            ),
            ("document.rs", include_str!("document.rs")),
            ("file_dialog.rs", include_str!("file_dialog.rs")),
            ("file_io.rs", include_str!("file_io.rs")),
            ("file_tree.rs", include_str!("file_tree.rs")),
            ("find.rs", include_str!("find.rs")),
            ("git_version.rs", include_str!("git_version.rs")),
            ("ime.rs", include_str!("ime.rs")),
            ("kill_ring.rs", include_str!("kill_ring.rs")),
            ("main.rs", include_str!("main.rs")),
            ("menu_commands.rs", include_str!("menu_commands.rs")),
            ("open_document.rs", include_str!("open_document.rs")),
            ("pane_layout.rs", include_str!("pane_layout.rs")),
            ("pty.rs", include_str!("pty.rs")),
            ("quick_draft.rs", include_str!("quick_draft.rs")),
            ("saving.rs", include_str!("saving.rs")),
            ("searcher.rs", include_str!("searcher.rs")),
            ("session.rs", include_str!("session.rs")),
            ("shell.rs", include_str!("shell.rs")),
            ("shortcuts.rs", include_str!("shortcuts.rs")),
            ("terminal.rs", include_str!("terminal.rs")),
            ("terminal_session.rs", include_str!("terminal_session.rs")),
            ("text_blocks.rs", include_str!("text_blocks.rs")),
            ("tree_watch.rs", include_str!("tree_watch.rs")),
            ("wallpaper.rs", include_str!("wallpaper.rs")),
            ("wiring.rs", include_str!("wiring.rs")),
            ("word_marks.rs", include_str!("word_marks.rs")),
            ("writer.rs", include_str!("writer.rs")),
        ];
        // 画面に出ない日本語。
        let exempt = [
            "」に傍点］",                // 青空文庫の注記（記法）
            "」に二重丸傍点］",          // 同上
            "」に白丸傍点］",            // 同上
            "」にゴマ傍点］",            // 同上
            "」に丸傍点］",              // 同上
            "」に×傍点］",               // 同上
            "」に傍線］",                // 同上
            "」に二重傍線］",            // 同上
            "」に波線］",                // 同上
            "」に鎖線］",                // 同上
            "」に破線］",                // 同上
            "」の左に「",                // 左の注記（記法）
            "」の注記］",                // 同上
            "［＃縦中横］",              // 範囲の注記（記法）
            "［＃縦中横終わり］",        // 同上
            "［＃割り注］",              // 同上
            "［＃割り注終わり］",        // 同上
            "［＃小さな文字］",          // 同上
            "［＃小さな文字終わり］",    // 同上
            "［＃大きな文字］",          // 同上
            "［＃大きな文字終わり］",    // 同上
            "［＃ここで字下げ終わり］",  // 体裁の注記（記法）
            "ここから",                  // 同上（注記の中の言い方）
            "字下げ",                    // 同上
            "［＃改ページ］",            // 同上
            "［＃地付き］",              // 行の頭に置く地付き（記法）
            "地付き",                    // 同上
            "地から",                    // 同上
            "字上げ",                    // 同上
            "［＃{count}字下げ］",       // 行の頭に置く字下げ（記法）
            "［＃地から{cells}字上げ］", // 行の頭に置く地付き（記法）
            "(なし)",                    // 診断ログの見出し
            "- 箇条書き",                // 起動時の測定
            "日本語ABC123",              // 起動時の測定
            "DirectWrite縦書き: OK / layout {:.0}×{:.0}px / caret Δy {:.1}px", // 診断ログ
            "DirectWrite縦書き: NG / {error}", // 診断ログ
            "the editing area always holds one pane (要件 6.3)", // panicの文
        ];
        let japanese = |text: &str| {
            text.chars().any(|c| {
                matches!(c, '\u{3041}'..='\u{3096}' | '\u{30a1}'..='\u{30fa}' | '\u{4e00}'..='\u{9fff}')
            })
        };
        let mut unpaired = Vec::new();
        for (name, source) in sources {
            // 試験の塊（`#[cfg(test)]`の次の`mod … {`）から先は見ない。
            let end = source
                .match_indices("#[cfg(test)]")
                .map(|(at, _)| at)
                .find(|at| {
                    let next = source[*at..].lines().nth(1).unwrap_or("").trim_end();
                    next.starts_with("mod ") && next.ends_with('{')
                })
                .unwrap_or(source.len());
            let code = &source[..end];
            let bytes = code.as_bytes();
            let mut at = 0;
            while at < bytes.len() {
                let rest = &code[at..];
                if rest.starts_with("//") {
                    at += rest.find('\n').unwrap_or(rest.len());
                } else if rest.starts_with("r#\"") {
                    at += rest[3..].find("\"#").map_or(rest.len(), |close| close + 5);
                } else if rest.starts_with("'\"'") {
                    at += 3;
                } else if rest.starts_with("'\\\"'") {
                    at += 4;
                } else if rest.starts_with('"') {
                    let mut close = 1;
                    while close < rest.len() && rest.as_bytes()[close] != b'"' {
                        close += if rest.as_bytes()[close] == b'\\' {
                            2
                        } else {
                            1
                        };
                    }
                    let text = &rest[1..close.min(rest.len())];
                    let after = rest[(close + 1).min(rest.len())..].trim_start();
                    let paired = after
                        .strip_prefix(',')
                        .map(str::trim_start)
                        .and_then(|next| next.strip_prefix('"'))
                        .is_some_and(|next| !japanese(next.split('"').next().unwrap_or("")));
                    if japanese(text) && !paired && !exempt.contains(&text) {
                        let line = code[..at].lines().count();
                        unpaired.push(format!("{name}:{line}: {text}"));
                    }
                    at += close + 1;
                } else {
                    at += rest.chars().next().map_or(1, char::len_utf8);
                }
            }
        }
        assert!(
            unpaired.is_empty(),
            "no English for:\n{}",
            unpaired.join("\n")
        );
    }

    /// 国際化②: 英語を選べば、Rustの文も英語で出る（このスレッドだけ切り替える）。
    #[test]
    fn rust_messages_follow_the_language() {
        super::TEST_LANGUAGE.with(|held| held.set(Some(false)));
        assert_eq!(super::pick("保存しました", "Saved"), "Saved");
        assert_eq!(crate::found_status(0, None), "Not found");
        assert_eq!(crate::found_status(12, None), "12 found");
        assert_eq!(
            crate::file_tree::NameProblem::Empty.message(),
            "Enter a name"
        );
        assert_eq!(crate::saving::conflict_choices()[4], "Cancel");
        assert_eq!(crate::word_marks::no_mode(), "None");
        let over = crate::over_limit_message("abc");
        assert!(over.starts_with("Undid 3 characters"), "{over}");
        super::TEST_LANGUAGE.with(|held| held.set(Some(true)));
        assert_eq!(crate::found_status(0, None), "見つかりません");
        assert_eq!(crate::saving::conflict_choices()[4], "キャンセル");
        super::TEST_LANGUAGE.with(|held| held.set(None));
    }
}
