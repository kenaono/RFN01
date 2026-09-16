//! 要件 7.10: 紙の形で見る——印刷プレビューと、そこからの印刷。
//!
//! **プリンタは刷る前に何も見せない。**この面が無ければ、用紙と余白が合っているかを
//! 刷ってから知ることになる。そして**ここに映る1枚と、プリンタへ送る1枚は同じ処理が
//! 描いた同じ絵**である（`directwrite_render::print::draw_page_onto`）——プレビューは
//! 「紙に切る仕事」を目に見えるようにしたものであって、別の絵ではない。
//!
//! **紙のために組み直す。**画面の幅で組んだものをそのまま紙へ出せば、折り返しが紙に
//! 合わない。だから印刷用の組版器をもう一つ持ち、紙の行の長さで組む。編集画面は
//! 続いた1枚のまま、何も変わらない。
use std::rc::Rc;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

use crate::directwrite_render::print::{self, Destination, Paper};
use crate::directwrite_render::{LineFit, TextEngine, WritingMode};
use crate::document::{self, PreviewDocument};
use crate::open_document::OpenDocument;
use crate::text_blocks::{StyledText, Typography};
use crate::{AppWindow, Live, StatusBar, focused_pane, pictures, say};

/// 紙に切った文書、いま見ている枚数つき。
pub struct Preview {
    engine: TextEngine,
    paper: Paper,
    pages: usize,
    /// **絵の細かさ**。紙のDIPに対する倍率で、画面に映すぶんだけ大きく描く。
    scale: f32,
    document: Rc<OpenDocument>,
}

/// プレビューの絵をどれだけ大きく描くか。
///
/// A4なら約1190×1684画素。**窓より大きく描いて縮めて見せる**ので、窓を広げても
/// 字が粗くならない。
const PREVIEW_SCALE: f32 = 1.5;

/// ☰の Print… と Ctrl+P。
pub fn open(window: &AppWindow, live: &Live) {
    let document = live.active(window);
    // **プレビューは実際の紙を映す。**既定のプリンタが送る紙の大きさを先に訊いて
    // おく（要件 7.10：用紙はWindowsに訊く）。プリンタが1台も無ければA4縦。
    let paper = print::default_printer()
        .map(|printer| printer.paper())
        .unwrap_or_default();
    let mode = mode_of(window);
    let engine = match lay_out(window, live, &document, mode, paper) {
        Ok(engine) => engine,
        Err(error) => {
            window.tell_tab(
                say!(
                    "紙に組めませんでした",
                    "The document could not be laid out on paper"
                )
                .into(),
            );
            live.cache
                .borrow_mut()
                .log_diag("print", &format!("layout failed: {error}"));
            return;
        }
    };
    let pages = print::page_count(&engine, paper);
    let title = document.file.borrow().title();
    *live.preview.borrow_mut() = Some(Preview {
        engine,
        paper,
        pages,
        scale: PREVIEW_SCALE,
        document,
    });
    window.set_print_title(title.into());
    window.set_print_pages(pages as i32);
    window.set_print_at(0);
    window.set_print_status(Default::default());
    window.set_print_aspect(paper.width / paper.height);
    window.set_print_active(true);
    draw(window, live);
}

/// 紙を繰る。範囲の外は繰らない（端で押しても何も起きない）。
pub fn turn(window: &AppWindow, live: &Live, to: i32) {
    let pages = live
        .preview
        .borrow()
        .as_ref()
        .map_or(0, |preview| preview.pages) as i32;
    if to < 0 || to >= pages || to == window.get_print_at() {
        return;
    }
    window.set_print_at(to);
    draw(window, live);
}

pub fn close(window: &AppWindow, live: &Live) {
    window.set_print_active(false);
    // **組版器はここで捨てる。**紙のぶんの組版は編集には要らないし、次に開くときの
    // 文書は同じとは限らない。
    *live.preview.borrow_mut() = None;
    crate::restore_editor_focus(window);
}

/// いま見ている紙を描いて、窓へ渡す。
fn draw(window: &AppWindow, live: &Live) {
    let at = window.get_print_at().max(0) as usize;
    let mut held = live.preview.borrow_mut();
    let Some(preview) = held.as_mut() else {
        return;
    };
    match print::render_page(&mut preview.engine, preview.paper, at, preview.scale) {
        Ok((pixels, width, height)) => {
            window.set_print_page(image_of(&pixels, width, height));
        }
        Err(error) => {
            live.cache
                .borrow_mut()
                .log_diag("print", &format!("page {at} failed: {error}"));
            window.set_print_status(
                say!("この紙を描けませんでした", "This sheet could not be drawn").into(),
            );
        }
    }
}

/// Direct2Dが返すBGRAを、窓が読む絵にする。
fn image_of(pixels: &[u8], width: u32, height: u32) -> Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    let bytes = buffer.make_mut_bytes();
    let wanted = bytes.len().min(pixels.len());
    bytes[..wanted].copy_from_slice(&pixels[..wanted]);
    // ビットマップはBGRA、窓が読むのはRGBA。**その場で入れ替える**（タイルと同じ）。
    for four in bytes.chunks_exact_mut(4) {
        four.swap(0, 2);
    }
    Image::from_rgba8_premultiplied(buffer)
}

/// 文書を紙の寸法で組む。
fn lay_out(
    window: &AppWindow,
    live: &Live,
    document: &OpenDocument,
    mode: WritingMode,
    paper: Paper,
) -> windows::core::Result<TextEngine> {
    let source = document.text.borrow().clone();
    let reading = crate::reading_of(window);
    let mut preview = PreviewDocument::default();
    // **編集行の原文表示はしない**（活性行は`None`）。紙に出るのは、どの行も
    // 組まれた姿である——原文が見たいときは画面で見る。
    preview.refresh(&source, None, reading);
    let folder = live.folder.borrow().root.clone();
    preview.size_images(|image| {
        let picture = pictures::load(&pictures::resolve(folder.as_deref(), image.target)?)?;
        // 絵は紙の寸法で入れる。**画面の拡大率は紙には効かない**——拡大は読むための
        // もので、刷る大きさではない。
        let size = pictures::size(&picture, image.width, 100);
        Some((picture, size))
    });
    // **行の体裁は原稿から読む。**プレビューの本文は行頭の注記（`［＃地付き］`）を
    // 既に取り除いてあるので、そちらから数えると地付きが消える。
    let styles = document::line_styles_reading(&source, reading);
    let styled =
        StyledText::marked(&preview.text, &styles, preview.marks()).with_markers(preview.markers());
    let (_, cross) = paper.printable(mode);
    let mut engine = TextEngine::new(mode);
    engine.set_pictures(preview.pictures().clone());
    engine.update(
        styled,
        LineFit::Extent(cross.round().max(1.0) as u32),
        &for_paper(&crate::pane_typography(window, focused_pane(window))),
    )?;
    Ok(engine)
}

/// 画面の体裁を、紙の体裁に直す。
///
/// **変えるのは色と、画面のための印だけ。**字体・大きさ・行送り・ルビの設定は
/// 書き手のものなので、そのまま紙へ持っていく（要件 7.10：画面と同じ結果を出す）。
///
/// 色は紙の側が持っている——白い紙に黒い字である。画面の紙はアイボリーで、墨も紙も
/// 書き手が選べるが（要件 9）、それは**光る面を長く見るための選択**であって、紙に
/// 焼く色ではない。行番号と空白の印は、読むためではなく編むための印なので出さない。
fn for_paper(screen: &Typography) -> Typography {
    Typography {
        paper: [1.0, 1.0, 1.0],
        ink: [0.0, 0.0, 0.0],
        heading_ink: [[0.0, 0.0, 0.0]; crate::MAX_HEADING_LEVEL],
        backgrounds: [[1.0, 1.0, 1.0]; 7],
        paper_painted: false,
        line_numbers: false,
        whitespace: false,
        ..screen.clone()
    }
}

/// 「Print…」を押したとき。
///
/// **プリンタ・部数・用紙はWindowsが訊く**（要件 7.10）。選ばれた紙がプレビューの
/// 紙と違えば、**組み直してから刷る**——見たとおりに出ないなら、プレビューは嘘を
/// ついたことになる。
pub fn print_now(window: &AppWindow, live: &Live) {
    let Some(chosen) = print::ask(crate::ime::window_handle(window).unwrap_or_default()) else {
        // 取り消しは何事も無かったことである。
        return;
    };
    let title = {
        let held = live.preview.borrow();
        held.as_ref()
            .map(|preview| preview.document.file.borrow().title())
            .unwrap_or_default()
    };
    window.set_print_working(true);
    window.set_print_status(say!("印刷しています…", "Printing…").into());
    let outcome = print_pages(window, live, &chosen);
    window.set_print_working(false);
    match outcome {
        Ok(pages) => {
            live.cache.borrow_mut().log_diag(
                "print",
                &format!("printed {pages} pages to {} ({title})", chosen.printer),
            );
            window.set_print_status(Default::default());
            close(window, live);
            let printer = &chosen.printer;
            window.tell_tab(
                if crate::i18n::japanese() {
                    format!("{printer}へ{pages}枚送りました")
                } else {
                    format!("Sent {pages} pages to {printer}")
                }
                .into(),
            );
        }
        Err(error) => {
            live.cache
                .borrow_mut()
                .log_diag("print", &format!("failed: {error}"));
            window.set_print_status(
                say!(
                    "プリンタへ送れませんでした",
                    "The printer would not take the job"
                )
                .into(),
            );
        }
    }
}

/// 選ばれたプリンタの紙に合わせ直して、全ページを送る。
fn print_pages(
    window: &AppWindow,
    live: &Live,
    chosen: &print::Chosen,
) -> windows::core::Result<usize> {
    let paper = chosen.paper();
    let same = {
        let held = live.preview.borrow();
        held.as_ref().is_some_and(|preview| {
            (preview.paper.width - paper.width).abs() < 1.0
                && (preview.paper.height - paper.height).abs() < 1.0
        })
    };
    if !same {
        // 紙が違えば折り返しも違う。**開き直す**のではなく、同じ文書を新しい紙で
        // 組み直して、プレビューもその紙になる。
        reopen(window, live, paper)?;
    }
    let mut held = live.preview.borrow_mut();
    let Some(preview) = held.as_mut() else {
        return Ok(0);
    };
    print::print(
        &mut preview.engine,
        preview.paper,
        Destination::Printer(chosen),
    )
}

/// この紙で組み直し、プレビューも新しい紙にする。
fn reopen(window: &AppWindow, live: &Live, paper: Paper) -> windows::core::Result<()> {
    let document = {
        let held = live.preview.borrow();
        let Some(preview) = held.as_ref() else {
            return Ok(());
        };
        preview.document.clone()
    };
    let mode = mode_of(window);
    let engine = lay_out(window, live, &document, mode, paper)?;
    let pages = print::page_count(&engine, paper);
    *live.preview.borrow_mut() = Some(Preview {
        engine,
        paper,
        pages,
        scale: PREVIEW_SCALE,
        document,
    });
    window.set_print_pages(pages as i32);
    window.set_print_at(window.get_print_at().min(pages as i32 - 1).max(0));
    window.set_print_aspect(paper.width / paper.height);
    draw(window, live);
    Ok(())
}

fn mode_of(window: &AppWindow) -> WritingMode {
    if focused_pane(window).vertical(window) {
        WritingMode::Vertical
    } else {
        WritingMode::Horizontal
    }
}
