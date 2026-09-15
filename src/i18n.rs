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
    SHOWING_JAPANESE.load(Ordering::Relaxed)
}

/// 日本語と英語の対から、いまの言語のほうを選ぶ。
pub fn pick<'a>(japanese_text: &'a str, english_text: &'a str) -> &'a str {
    if japanese() {
        japanese_text
    } else {
        english_text
    }
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
            include_str!("../ui/controls.slint"),
            include_str!("../ui/diff-window.slint"),
            include_str!("../ui/quick-draft.slint"),
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
}
