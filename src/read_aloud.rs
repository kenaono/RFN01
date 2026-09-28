//! 読み上げで校正する（RFN01-62、書き手と合意 2026-09-28）。
//!
//! 原稿を耳で聞いて直す——印刷（要件 7.10）と同じく「目で読むのとは別の感覚で
//! 気づく」ための道具。**声はWindowsのもの**（WinRTの`SpeechSynthesizer`）で、
//! 自前の音声は持たない。
//!
//! - 実行メニューの「読み上げ」で、**キャレットの位置から文書の終わりまで**、
//!   段落（改行から改行まで）を1つずつ読む。何を声にするかは
//!   [`crate::document::spoken_paragraphs`]の1か所が決める。
//! - 読んでいる段落の地を塗り（ブックマークの帯と同じ）、キャレットをその頭へ
//!   移して画面を付いて行かせる。
//! - 止まるのは「読み上げを停止」、本文を書き換えたとき、タブを閉じる・移ったとき、
//!   最後まで読んだとき。
//!
//! **段落ごとに合成して鳴らす。**1段落の合成は数十ms（69字で58ms）なので、読んで
//! いるあいだに次の段落を合成しておき、鳴り終わったら続けて鳴らす。
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use windows::Foundation::TypedEventHandler;
use windows::Media::Core::MediaSource;
use windows::Media::Playback::MediaPlayer;
use windows::Media::SpeechSynthesis::{SpeechSynthesisStream, SpeechSynthesizer, VoiceInformation};
use windows::Storage::Streams::IRandomAccessStream;
use windows::core::{HSTRING, Interface};
use windows_future::IAsyncOperation;

use crate::document::{self, Spoken};
use crate::i18n::pick;
use crate::open_document::OpenDocument;
use crate::{AppWindow, Live, PaneId, StatusBar};

/// 速さの選び方（書き手と合意：0.75×〜2.0×）。設定が持つのは番号。
pub const RATES: [f64; 5] = [0.75, 1.0, 1.25, 1.5, 2.0];
/// 速さの既定（1.0×）。
pub const DEFAULT_RATE: i32 = 1;

/// Windowsに入っている声の1つ。
#[derive(Clone, Debug)]
pub struct Voice {
    pub id: String,
    pub name: String,
    pub language: String,
}

/// Windowsに入っている声（設定の一覧と、使えるかどうかの判定）。
pub fn voices() -> Vec<Voice> {
    let Ok(all) = SpeechSynthesizer::AllVoices() else {
        return Vec::new();
    };
    all.into_iter()
        .filter_map(|voice| {
            Some(Voice {
                id: voice.Id().ok()?.to_string(),
                name: voice.DisplayName().ok()?.to_string(),
                language: voice.Language().ok()?.to_string(),
            })
        })
        .collect()
}

/// **日本語の声が1つでも入っていれば使える**（書き手と合意：入っている言語で使用可否を
/// 決め、無ければ無効）。
pub fn available(voices: &[Voice]) -> bool {
    voices
        .iter()
        .any(|voice| voice.language.to_ascii_lowercase().starts_with("ja"))
}

/// 読んでいるあいだの状態。
struct Reading {
    pane: PaneId,
    document: Weak<OpenDocument>,
    paragraphs: Vec<Spoken>,
    /// いま鳴っている段落。
    at: usize,
    synthesizer: SpeechSynthesizer,
    player: MediaPlayer,
    /// 次の段落の合成（鳴っているあいだに進める）。
    next: Option<IAsyncOperation<SpeechSynthesisStream>>,
    /// 鳴り終わりの知らせが、どの読み上げのものか。止めたあとに届いた知らせは捨てる。
    generation: u64,
}

thread_local! {
    static READING: RefCell<Option<Reading>> = const { RefCell::new(None) };
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    static HOOK: RefCell<Option<(slint::Weak<AppWindow>, Live)>> = const { RefCell::new(None) };
}

/// 起動のときに1度：鳴り終わりと書き換えの知らせを、窓と文書につなぐ。
/// 声の一覧は設定を読んだあと（`apply_settings`）に出す。
pub fn install(window: &AppWindow, live: &Live) {
    HOOK.with(|held| *held.borrow_mut() = Some((window.as_weak(), live.clone())));
}

fn with_hook(f: impl FnOnce(&AppWindow, &Live)) {
    let held = HOOK.with(|held| held.borrow().clone());
    if let Some((weak, live)) = held
        && let Some(window) = weak.upgrade()
    {
        f(&window, &live);
    }
}

/// 読み上げているか。
pub fn reading() -> bool {
    READING.with(|held| held.borrow().is_some())
}

/// 設定の声の一覧と、選んでいる声を窓へ（Settings → General → READ ALOUD）。
///
/// 設定が持つのは声の**Id**（`speech-voice-id`、空ならWindowsの既定）。一覧の番号は
/// 声が入れ替わると動くので、番号では持たない。
pub fn publish_voices(window: &AppWindow) {
    let voices = voices();
    let usable = available(&voices);
    let chosen = window.get_speech_voice_id().to_string();
    let default_id = SpeechSynthesizer::DefaultVoice()
        .and_then(|voice| voice.Id())
        .map(|id| id.to_string())
        .unwrap_or_default();
    let wanted = if chosen.is_empty() {
        &default_id
    } else {
        &chosen
    };
    let index = voices
        .iter()
        .position(|voice| &voice.id == wanted)
        .or_else(|| voices.iter().position(|voice| voice.id == default_id))
        .map_or(-1, |at| at as i32);
    let names: Vec<SharedString> = voices
        .iter()
        .map(|voice| format!("{} ({})", voice.name, voice.language).into())
        .collect();
    window.set_speech_voice_names(ModelRc::new(VecModel::from(names)));
    window.set_speech_voice(index);
    window.set_speech_available(usable);
}

/// 設定で声を選んだ。
pub fn voice_chosen(window: &AppWindow, index: i32) {
    let voices = voices();
    if let Some(voice) = usize::try_from(index).ok().and_then(|at| voices.get(at)) {
        window.set_speech_voice_id(voice.id.clone().into());
        window.set_speech_voice(index);
    }
}

fn synthesizer_for(window: &AppWindow) -> windows::core::Result<SpeechSynthesizer> {
    let synthesizer = SpeechSynthesizer::new()?;
    let chosen = window.get_speech_voice_id().to_string();
    if !chosen.is_empty()
        && let Ok(all) = SpeechSynthesizer::AllVoices()
        && let Some(voice) = all
            .into_iter()
            .find(|voice: &VoiceInformation| voice.Id().is_ok_and(|id| id == chosen.as_str()))
    {
        synthesizer.SetVoice(&voice)?;
    }
    let rate = usize::try_from(window.get_speech_rate())
        .ok()
        .and_then(|at| RATES.get(at))
        .copied()
        .unwrap_or(1.0);
    synthesizer.Options()?.SetSpeakingRate(rate)?;
    Ok(synthesizer)
}

/// 実行の「読み上げ」／「読み上げを停止」（RFN01-62）。
pub fn toggle(window: &AppWindow, live: &Live, id: PaneId) {
    if reading() {
        stop(window, live);
        return;
    }
    if !available(&voices()) {
        window.tell(
            pick(
                "日本語の音声がWindowsに入っていないため、読み上げできません",
                "Read aloud needs a Japanese voice installed in Windows",
            )
            .into(),
        );
        return;
    }
    let document = live.states.document(id);
    let source = document.text.borrow().clone();
    let caret = live.states.of(id).borrow().caret_source_byte.unwrap_or(0);
    let paragraphs = document::spoken_paragraphs(&source, caret);
    if paragraphs.is_empty() {
        window.tell(pick("読む本文がありません", "Nothing to read").into());
        return;
    }
    let started = (|| -> windows::core::Result<Reading> {
        let synthesizer = synthesizer_for(window)?;
        let player = MediaPlayer::new()?;
        // 試験では鳴らさない（流れは同じに通す）。
        #[cfg(test)]
        player.SetIsMuted(true)?;
        let generation = GENERATION.with(|held| {
            held.set(held.get() + 1);
            held.get()
        });
        // **鳴り終わりは別のスレッドから来る。**窓のスレッドへ渡してから進める。
        player.MediaEnded(&TypedEventHandler::new(move |_, _| {
            let _ = slint::invoke_from_event_loop(move || ended(generation));
            Ok(())
        }))?;
        player.MediaFailed(&TypedEventHandler::new(move |_, _| {
            let _ = slint::invoke_from_event_loop(move || failed(generation));
            Ok(())
        }))?;
        Ok(Reading {
            pane: id,
            document: Rc::downgrade(&document),
            paragraphs,
            at: 0,
            synthesizer,
            player,
            next: None,
            generation,
        })
    })();
    let Ok(reading) = started else {
        window.tell(pick("読み上げを始められません", "Cannot start reading aloud").into());
        return;
    };
    READING.with(|held| *held.borrow_mut() = Some(reading));
    live.cache.borrow_mut().log_diag("spec.speech", "start");
    if !play(window, live, 0, None) {
        stop(window, live);
        window.tell(pick("読み上げを始められません", "Cannot start reading aloud").into());
    }
}

/// `at`の段落を鳴らし、次の段落の合成を始める。`ready`はもう合成してある流れ。
fn play(window: &AppWindow, live: &Live, at: usize, ready: Option<SpeechSynthesisStream>) -> bool {
    let shown = READING.with(|held| {
        let mut held = held.borrow_mut();
        let reading = held.as_mut()?;
        let paragraph = reading.paragraphs.get(at)?.clone();
        let stream = match ready {
            Some(stream) => stream,
            None => reading
                .synthesizer
                .SynthesizeTextToStreamAsync(&HSTRING::from(paragraph.text.as_str()))
                .ok()?
                .join()
                .ok()?,
        };
        let content = stream.ContentType().ok()?;
        let stream: IRandomAccessStream = stream.cast().ok()?;
        let source = MediaSource::CreateFromStream(&stream, &content).ok()?;
        reading.player.SetSource(&source).ok()?;
        reading.player.Play().ok()?;
        reading.at = at;
        reading.next = reading.paragraphs.get(at + 1).and_then(|next| {
            reading
                .synthesizer
                .SynthesizeTextToStreamAsync(&HSTRING::from(next.text.as_str()))
                .ok()
        });
        Some((reading.pane, reading.document.clone(), paragraph.range))
    });
    let Some((pane, document, range)) = shown else {
        return false;
    };
    let Some(document) = document.upgrade() else {
        return false;
    };
    // **キャレットを段落の頭へ**——画面が付いて行き、止めたときはそこから書ける。
    // 選択は作らない（塗るのは帯）。
    let source = document.text.borrow().clone();
    if range.start <= source.len() {
        let state = live.states.of(pane);
        crate::select_source_range(
            window,
            &live.cache,
            &document,
            &state,
            pane,
            &source,
            range.start,
            range.start,
        );
    }
    true
}

/// 1段落が鳴り終わった。次があれば続け、無ければ終える。
fn ended(generation: u64) {
    with_hook(|window, live| {
        let next = READING.with(|held| {
            let mut held = held.borrow_mut();
            let reading = held
                .as_mut()
                .filter(|reading| reading.generation == generation)?;
            let stream = reading.next.take().and_then(|pending| pending.join().ok());
            Some((reading.at + 1, stream))
        });
        let Some((at, stream)) = next else {
            return;
        };
        let more = READING.with(|held| {
            held.borrow()
                .as_ref()
                .is_some_and(|reading| at < reading.paragraphs.len())
        });
        if !more || !play(window, live, at, stream) {
            stop(window, live);
        }
    });
}

fn failed(generation: u64) {
    with_hook(|window, live| {
        let current = READING.with(|held| {
            held.borrow()
                .as_ref()
                .is_some_and(|reading| reading.generation == generation)
        });
        if current {
            stop(window, live);
            window.tell(pick("音声を再生できませんでした", "Could not play the voice").into());
        }
    });
}

/// 止める。塗った帯も外す。
pub fn stop(window: &AppWindow, live: &Live) {
    let Some(reading) = READING.with(|held| held.borrow_mut().take()) else {
        return;
    };
    let _ = reading.player.Pause();
    let _ = reading.player.Close();
    live.cache.borrow_mut().log_diag("spec.speech", "stop");
    crate::relayout_panes(window, &live.states, &live.cache);
}

/// 本文が変わった（`DocumentEvents::editing`）。読んでいれば止める。
///
/// **その場では止めない**——書き換えの途中（本文を借りている最中）に呼ばれるので、
/// 描き直しは書き換えが済んでから。
pub fn touched() {
    if !reading() {
        return;
    }
    slint::Timer::single_shot(Duration::ZERO, || {
        with_hook(stop);
    });
}

/// 前に出ているものが替わった（タブを移った・閉じた）。読んでいる文書がその
/// ペインの前にもう無ければ止める。
pub fn check_front(window: &AppWindow, live: &Live) {
    let Some((pane, document)) = READING.with(|held| {
        held.borrow()
            .as_ref()
            .map(|reading| (reading.pane, reading.document.clone()))
    }) else {
        return;
    };
    let still = document.upgrade().is_some_and(|document| {
        std::ptr::eq(
            Rc::as_ptr(&live.states.document(pane)),
            Rc::as_ptr(&document),
        )
    });
    if !still {
        stop(window, live);
    }
}

/// 読んでいる段落（`id`のペインがその文書を出しているとき）。組版が地を塗る。
pub fn mark_in(id: PaneId, document: &OpenDocument) -> Option<(usize, usize)> {
    READING.with(|held| {
        let held = held.borrow();
        let reading = held.as_ref()?;
        let same = reading.pane == id
            && reading
                .document
                .upgrade()
                .is_some_and(|held| std::ptr::eq(Rc::as_ptr(&held), document));
        let range: &Range<usize> = &reading.paragraphs.get(reading.at)?.range;
        same.then_some((range.start, range.end))
    })
}

/// 試験のため：いま読んでいる段落の番号。
#[cfg(test)]
pub fn reading_at() -> Option<usize> {
    READING.with(|held| held.borrow().as_ref().map(|reading| reading.at))
}

/// 試験のため：鳴り終わったことにする。
#[cfg(test)]
pub fn finish_paragraph_for_test() {
    let generation = READING.with(|held| held.borrow().as_ref().map(|reading| reading.generation));
    if let Some(generation) = generation {
        ended(generation);
    }
}
