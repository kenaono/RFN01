//! 英語の綴りの確認の、Windowsと画面の側（RFN01-63）。
//!
//! 本文の読み方と数え方は[`crate::spelling`]にある。ここは**Windowsのスペルチェック
//! （`ISpellChecker`、en-US）に語を聞く**ことと、Runメニュー・右クリック・ステータス
//! バー・書いたあとの数え直しをつなぐことだけをする。
//!
//! **書き手が頼んだ文書だけを調べる**（書き手と合意 2026-09-28）。Runの「Check
//! Spelling」で始まり、「Stop Checking Spelling」かタブを閉じると終わる。再起動では
//! 戻さない。
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Globalization::{ISpellChecker, ISpellCheckerFactory, SpellCheckerFactory};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::core::{HSTRING, PWSTR};

use crate::i18n::pick;
use crate::open_document::OpenDocument;
use crate::spelling::{self, DocumentSpelling, SpellMarks};
use crate::{AppWindow, Live, PaneId, StatusBar};

/// 候補の数（書き手と合意：最大3つ）。
const SUGGESTIONS: usize = 3;

/// 書いたあと、数え直すまで待つ時間。**打っている最中は数えない**——語を打ち終えて
/// 手が止まったところで印が付く。
const SETTLE: Duration = Duration::from_millis(500);

/// Windowsのスペルチェック（en-US）と、聞いた答え。
///
/// **答えは語ごとに覚える。**1語0.1msでも、文書を数え直すたびに全部の語を聞けば
/// 10万字で数秒になる（RFN01-63の調査）。同じ語は2度聞かない。
struct Checker {
    inner: ISpellChecker,
    known: HashMap<String, bool>,
}

impl Checker {
    fn open() -> Option<Self> {
        // SAFETY: COMはこのスレッド（窓のスレッド）で使う。既に初期化してあれば
        // 何もしない呼び出しになる。
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let factory: ISpellCheckerFactory =
                CoCreateInstance(&SpellCheckerFactory, None, CLSCTX_INPROC_SERVER).ok()?;
            let language = HSTRING::from("en-US");
            if !factory.IsSupported(&language).ok()?.as_bool() {
                return None;
            }
            let inner = factory.CreateSpellChecker(&language).ok()?;
            Some(Self {
                inner,
                known: HashMap::new(),
            })
        }
    }

    fn is_wrong(&mut self, word: &str) -> bool {
        if let Some(wrong) = self.known.get(word) {
            return *wrong;
        }
        // SAFETY: 列挙子はこの呼び出しの中でだけ使う。
        let wrong = unsafe {
            self.inner
                .Check(&HSTRING::from(word))
                .ok()
                .is_some_and(|errors| {
                    let mut error = None;
                    errors.Next(&mut error) == S_OK && error.is_some()
                })
        };
        self.known.insert(word.to_owned(), wrong);
        wrong
    }

    fn suggest(&self, word: &str) -> Vec<String> {
        let mut out = Vec::new();
        // SAFETY: 受け取った文字列はCOMの割り当てなので、写してから返す。
        unsafe {
            let Ok(list) = self.inner.Suggest(&HSTRING::from(word)) else {
                return out;
            };
            while out.len() < SUGGESTIONS {
                let mut item = [PWSTR::null()];
                let mut fetched = 0;
                if list.Next(&mut item, Some(&mut fetched)) != S_OK || fetched == 0 {
                    break;
                }
                if let Ok(text) = item[0].to_string() {
                    out.push(text);
                }
                CoTaskMemFree(Some(item[0].0 as _));
            }
        }
        out
    }

    /// Windowsの辞書へ足す。**他のアプリとも共有される**（書き手と合意）。
    fn add(&mut self, word: &str) {
        // SAFETY: 文字列は呼び出しのあいだ生きている。
        let _ = unsafe { self.inner.Add(&HSTRING::from(word)) };
        self.known.insert(word.to_owned(), false);
    }
}

/// 右クリックした語（メニューの行が押されるまで持つ）。
struct Pointed {
    document: Weak<OpenDocument>,
    pane: PaneId,
    /// ソースのバイト範囲。
    range: Range<usize>,
    word: String,
    repeated: bool,
    suggestions: Vec<String>,
}

thread_local! {
    static CHECKER: RefCell<Option<Option<Checker>>> = const { RefCell::new(None) };
    static POINTED: RefCell<Option<Pointed>> = const { RefCell::new(None) };
    /// 書いたあとに数え直す時計。**押し直せば待ち直す**（`start`は前の待ちを捨てる）。
    static SETTLING: slint::Timer = slint::Timer::default();
    static RECOUNT: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
    /// 確認している文書があるかもしれない。無ければ書くたびに時計を回さない。
    static CHECKING: Cell<bool> = const { Cell::new(false) };
}

/// 聞き手を使う。Windowsに英語のスペルチェックが無ければ`None`。
fn with_checker<T>(f: impl FnOnce(&mut Checker) -> T) -> Option<T> {
    CHECKER.with(|held| {
        let mut held = held.borrow_mut();
        held.get_or_insert_with(Checker::open).as_mut().map(f)
    })
}

/// Windowsに英語のスペルチェックがあるか。
pub fn available() -> bool {
    with_checker(|_| ()).is_some()
}

/// 起動のときに1度：書いたあとの数え直しを、窓と文書の一覧につなぐ。
pub fn install(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    let live = live.clone();
    let recount: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(window) = weak.upgrade() {
            recount_all(&window, &live);
        }
    });
    RECOUNT.with(|held| *held.borrow_mut() = Some(recount));
}

/// 本文が変わった（`DocumentEvents::editing`）。確認している文書があれば、手が
/// 止まってから数え直す。
pub fn touched() {
    if !CHECKING.with(Cell::get) {
        return;
    }
    let Some(recount) = RECOUNT.with(|held| held.borrow().clone()) else {
        return;
    };
    SETTLING.with(|timer| {
        timer.start(slint::TimerMode::SingleShot, SETTLE, move || recount());
    });
}

/// 確認している文書をすべて数え直し、描き直す。
pub(crate) fn recount_all(window: &AppWindow, live: &Live) {
    let mut any = false;
    for document in crate::open_documents(live) {
        if document.spelling.borrow().is_some() {
            any = true;
            recount(window, &document);
        }
    }
    CHECKING.with(|checking| checking.set(any));
    publish(window, live);
    crate::relayout_panes(window, &live.states, &live.cache);
}

/// 1つの文書を数え直す。**答えが変わらなければ印の指紋も変えない**——変えると
/// 見えているタイルが全部描き直される。
fn recount(window: &AppWindow, document: &OpenDocument) {
    let source = document.text.borrow().clone();
    let ignored = match document.spelling.borrow().as_ref() {
        Some(held) => held.ignored.clone(),
        None => return,
    };
    let found = {
        let mut counts = document.counts.borrow_mut();
        let styles = counts.get(&source, crate::reading_of(window)).line_styles();
        with_checker(|checker| {
            spelling::scan(&source, styles, &ignored, |word| checker.is_wrong(word))
        })
    };
    let Some(found) = found else {
        return;
    };
    let mut held = document.spelling.borrow_mut();
    let Some(held) = held.as_mut() else {
        return;
    };
    let marks = SpellMarks::new(found.wrong, ignored);
    if marks.fingerprint() != held.marks.fingerprint() {
        held.marks = Arc::new(marks);
    }
    held.count = found.count;
}

/// Runの「Check Spelling」／「Stop Checking Spelling」（RFN01-63）。
///
/// **アクティブな文書に対して**。確認していれば終え、していなければ始める。
pub fn toggle(window: &AppWindow, live: &Live, id: PaneId) {
    let document = live.states.document(id);
    if document.spelling.borrow().is_some() {
        *document.spelling.borrow_mut() = None;
        forget_pointed();
    } else {
        if document.read_only() || id.screen(window).viewer {
            window.tell(
                pick(
                    "ViewerとReadOnlyでは綴りを確認できません",
                    "Spelling cannot be checked in Viewer or ReadOnly",
                )
                .into(),
            );
            return;
        }
        if !available() {
            window.tell(
                pick(
                    "Windowsの英語のスペルチェックを使えません",
                    "Windows' English spell checker is not available",
                )
                .into(),
            );
            return;
        }
        *document.spelling.borrow_mut() = Some(DocumentSpelling::default());
        CHECKING.with(|checking| checking.set(true));
        recount(window, &document);
        let count = document
            .spelling
            .borrow()
            .as_ref()
            .map_or(0, |held| held.count);
        if count == 0 {
            window.tell(pick("綴りの誤りはありません", "No spelling errors").into());
        }
        live.cache
            .borrow_mut()
            .log_diag("spec.spelling", &format!("start count={count}"));
    }
    publish(window, live);
    crate::relayout_panes(window, &live.states, &live.cache);
}

/// 前に出ている文書を確認しているか（Runの行の言葉を選ぶ）。
pub fn checking(document: &OpenDocument) -> bool {
    document.spelling.borrow().is_some()
}

/// ステータスバーの数（要件 10）。**確認しているあいだずっと**出す。
pub fn publish(window: &AppWindow, live: &Live) {
    let document = live.states.document(crate::focused_pane(window));
    let text = match document.spelling.borrow().as_ref() {
        Some(held) => crate::say!("綴りの誤り {}", "Spelling {}", held.count),
        None => String::new(),
    };
    window.set_spelling_status(SharedString::from(text));
}

fn forget_pointed() {
    POINTED.with(|held| *held.borrow_mut() = None);
}

fn clear_rows(window: &AppWindow) {
    window.set_spell_word(SharedString::new());
    window.set_spell_repeated(false);
    window.set_spell_suggestions(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
}

/// 右クリックした所の語（RFN01-63）。印の付いた語なら、メニューの先頭の行を用意する。
///
/// **対象はキャレットではなく右クリックした位置**（書き手と合意）。右クリックは
/// キャレットを動かさない。
pub fn at_pointer(window: &AppWindow, live: &Live, id: PaneId, x: f32, y: f32) {
    forget_pointed();
    clear_rows(window);
    if id.screen(window).viewer {
        return;
    }
    let Some((document, source, letter)) = crate::letter_at(window, live, id, x, y) else {
        return;
    };
    let marks = match document.spelling.borrow().as_ref() {
        Some(held) => held.marks.clone(),
        None => return,
    };
    let line_start = source[..letter].rfind('\n').map_or(0, |at| at + 1);
    let line_end = source[letter..]
        .find('\n')
        .map_or(source.len(), |at| letter + at);
    // 数えないところ（コード・表）は、印が無いので行も出さない。
    let line_index = source[..line_start].matches('\n').count();
    let skipped = {
        let mut counts = document.counts.borrow_mut();
        let styles = counts.get(&source, crate::reading_of(window)).line_styles();
        styles.get(line_index).is_some_and(|style| {
            style.kind.is_code()
                || matches!(
                    style.kind,
                    crate::text_blocks::LineKind::TableRow
                        | crate::text_blocks::LineKind::TableRule
                )
        })
    };
    if skipped {
        return;
    }
    let line = &source[line_start..line_end];
    let Some((range, repeated)) = spelling::word_at(line, letter - line_start, &marks) else {
        return;
    };
    let word = line[range.clone()].to_owned();
    let suggestions = if repeated {
        Vec::new()
    } else {
        with_checker(|checker| checker.suggest(&word)).unwrap_or_default()
    };
    window.set_spell_word(word.clone().into());
    window.set_spell_repeated(repeated);
    let rows: Vec<SharedString> = suggestions.iter().map(SharedString::from).collect();
    window.set_spell_suggestions(ModelRc::new(VecModel::from(rows)));
    POINTED.with(|held| {
        *held.borrow_mut() = Some(Pointed {
            document: Rc::downgrade(&document),
            pane: id,
            range: line_start + range.start..line_start + range.end,
            word,
            repeated,
            suggestions,
        });
    });
}

/// 右クリックの行が押された（RFN01-63）。`action`は候補の番号（0から）、
/// `-1`＝繰り返しの語を消す、`-2`＝Ignore、`-3`＝Add to Dictionary。
pub fn chosen(window: &AppWindow, live: &Live, action: i32) {
    let Some(pointed) = POINTED.with(|held| held.borrow_mut().take()) else {
        return;
    };
    clear_rows(window);
    let Some(document) = pointed.document.upgrade() else {
        return;
    };
    let source = document.text.borrow().clone();
    // **押すまでに本文が変わっていたら何もしない**——別の語を書き換える。
    if source.get(pointed.range.clone()) != Some(pointed.word.as_str()) {
        return;
    }
    match action {
        -2 => {
            if let Some(held) = document.spelling.borrow_mut().as_mut() {
                held.ignored.insert(pointed.word.clone());
            }
            recount_all(window, live);
        }
        -3 => {
            with_checker(|checker| checker.add(&pointed.word));
            recount_all(window, live);
        }
        -1 if pointed.repeated => {
            // 前の語とのあいだの空白ごと消す。
            let start = source[..pointed.range.start]
                .trim_end_matches([' ', '\t'])
                .len();
            let region = start..pointed.range.end;
            crate::apply_span_edit(
                window,
                live,
                pointed.pane,
                &source,
                region,
                "",
                (start, start),
                "spelling delete-repeated",
            );
        }
        at if at >= 0 => {
            let Some(replacement) = pointed.suggestions.get(at as usize) else {
                return;
            };
            let end = pointed.range.start + replacement.len();
            crate::apply_span_edit(
                window,
                live,
                pointed.pane,
                &source,
                pointed.range.clone(),
                replacement,
                (end, end),
                "spelling replace",
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFN01-63の調査で確かめた答えが、この道でも返る（Windowsのen-US）。
    #[test]
    fn windows_answers_for_english_words() {
        let Some((wrong, right, suggested)) = with_checker(|checker| {
            (
                checker.is_wrong("recieve"),
                checker.is_wrong("receive"),
                checker.suggest("recieve"),
            )
        }) else {
            eprintln!("en-US spell checker not installed; skipped");
            return;
        };
        assert!(wrong);
        assert!(!right);
        assert_eq!(suggested.first().map(String::as_str), Some("receive"));
        assert!(suggested.len() <= SUGGESTIONS);
    }
}
