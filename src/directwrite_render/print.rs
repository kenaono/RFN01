//! 要件 7.10: 紙の形で見る——印刷。
//!
//! **画面と同じ描画を紙へ出す。**組版はDirectWriteが既にしているので、紙のために
//! 別の組版器を持ち込まない——二つ持てば、画面と紙で違う原稿になる。ここにあるのは
//! 「どこで紙を切るか」と「Windowsの印刷へどう渡すか」だけで、字を並べる処理は
//! [`super::draw_block`]、つまり画面のタイルが使っているものそのままである。
//!
//! **PDFはプリンタの一つ**（「Microsoft Print to PDF」）であって、PDFをこちらで
//! 書くことはしない。字の埋め込みも縦書きの字形の差し替えもWindowsの側に既にあり、
//! そこを自作すれば、やはり画面と紙で違う原稿になる。
//!
//! **画面からの入口はまだ無い**（2026-09-16）。ここまでが「道が使えるか」を確かめる
//! ぶんで、試験（`print_tests`）が呼ぶ。入口——印刷プレビューとプリンタの選択——は
//! 次に作る（追加要件）。
#![allow(dead_code)]

use std::path::Path;

use windows::{
    Win32::{
        Foundation::{E_FAIL, GlobalFree, HGLOBAL, HMODULE, HWND},
        Graphics::{
            Direct2D::{
                Common::{D2D_RECT_F, D2D_SIZE_F},
                D2D1_ANTIALIAS_MODE_ALIASED, D2D1_COLOR_SPACE_SRGB,
                D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
                D2D1_PRINT_CONTROL_PROPERTIES, D2D1_PRINT_FONT_SUBSET_MODE_DEFAULT,
                ID2D1CommandList, ID2D1DeviceContext, ID2D1Factory1, ID2D1RenderTarget,
            },
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device,
            },
            DirectWrite::{
                DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_CENTER,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            },
            Dxgi::IDXGIDevice,
            Gdi::{
                DEVMODEA, DEVMODEW, DeleteDC, GetDeviceCaps, HDC, LOGPIXELSX, LOGPIXELSY,
                PHYSICALHEIGHT, PHYSICALWIDTH,
            },
            Imaging::{IWICBitmapSource, WICRect},
            Printing::PrintTicket::{
                PTCloseProvider, PTConvertDevModeToPrintTicket, PTOpenProvider, kPTJobScope,
            },
        },
        Storage::Xps::Printing::{
            IPrintDocumentPackageTargetFactory, PrintDocumentPackageTargetFactory,
        },
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, CoCreateInstance, IStream, STGM_CREATE, STGM_READWRITE,
                STREAM_SEEK_SET, StructuredStorage::CreateStreamOnHGlobal,
            },
            Memory::{GlobalLock, GlobalUnlock},
        },
        UI::{
            Controls::Dialogs::{
                DEVNAMES, PD_NOPAGENUMS, PD_NOSELECTION, PD_RETURNDEFAULT, PD_RETURNIC,
                PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGEX_FLAGS, PRINTDLGW, PrintDlgW,
            },
            Shell::SHCreateStreamOnFileEx,
        },
    },
    core::{Error, HSTRING, Interface, PCWSTR, Result},
};

use super::{Graphics, Inks, TextEngine, WritingMode, colour, draw_block, with_graphics};
use crate::text_blocks::{CrossSlices, FlowOrder, LineOrnament, TileSpan};

/// Direct2Dが長さを数える単位（1/96インチ）でのミリメートル。
///
/// **紙の大きさはミリで言うほうが人に通じる**ので、書くときはミリで書いて、ここで
/// 直す。インチで持つと、A4が8.27インチという読みにくい数になる。
pub const MM: f32 = 96.0 / 25.4;

/// 一枚の紙。
///
/// **余白はこの編集器が持つ**（要件 7.10）。用紙の大きさと向きはWindowsのプリンタが
/// 決めるので、いずれそちらから来る——それまでは既定のA4縦を使う。
#[derive(Clone, Copy, Debug)]
pub struct Paper {
    /// 紙の幅と高さ。
    pub width: f32,
    pub height: f32,
    /// 四辺の余白。
    pub margin: f32,
}

impl Default for Paper {
    /// A4縦、四辺20mm。**試作の値**で、既定は書き手が刷ったものを見てから決める
    /// （追加要件）。
    fn default() -> Self {
        Self {
            width: 210.0 * MM,
            height: 297.0 * MM,
            margin: 20.0 * MM,
        }
    }
}

impl Paper {
    /// 本文が入る範囲を、流れの軸と行の軸で。
    ///
    /// **どちらが幅になるかは組み方で入れ替わる**——縦書きは行が縦に立って右へ
    /// 流れるので、流れの軸が紙の幅である。
    pub fn printable(&self, mode: WritingMode) -> (f32, f32) {
        let width = (self.width - self.margin * 2.0).max(1.0);
        let height = (self.height - self.margin * 2.0).max(1.0);
        match mode {
            WritingMode::Vertical => (width, height),
            WritingMode::Horizontal => (height, width),
        }
    }
}

/// 出力先。
pub enum Destination<'a> {
    /// 書き手が選んだプリンタへ、Windowsに訊いた設定のまま送る。
    Printer(&'a Chosen),
    /// 「Microsoft Print to PDF」にこのファイルを書かせる。
    ///
    /// **PDF専用の道ではない**——同じプリンタに、訊かずに書く先を教えているだけで
    /// ある（出力先を渡さなければ、プリンタ自身が保存先を訊く）。
    PdfFile(&'a Path),
}

/// Windowsに入っているPDFのプリンタ。**Windowsの機能なので、切ってあれば無い。**
pub const PDF_PRINTER: &str = "Microsoft Print to PDF";

/// 組み終わった文書を紙に切り、Windowsの印刷へ流す。刷った枚数を返す。
///
/// `engine`は**紙の寸法で組んであること**——画面の幅で組んだものをそのまま紙へ
/// 出せば、折り返しが紙に合わない。切る前に[`page_count`]と同じ寸法で
/// `update`しておく。
pub fn print(engine: &mut TextEngine, paper: Paper, to: Destination<'_>) -> Result<usize> {
    let pages = page_count(engine, paper);
    if pages == 0 {
        return Ok(0);
    }
    let (printer, file, ticket) = match to {
        Destination::Printer(chosen) => (chosen.printer.clone(), None, chosen.ticket.clone()),
        Destination::PdfFile(path) => (PDF_PRINTER.to_owned(), Some(path), None),
    };
    with_graphics(|graphics| {
        // 1.1の口はDXGIのデバイスから作る。**絵を描くためではない**——画面の組版は
        // 今までどおりWICのビットマップに描いていて、この道は印刷の口を開けるため
        // だけに通る。
        let mut d3d: Option<ID3D11Device> = None;
        // SAFETY: The out parameter is the only one asked for, and it is read
        // back through the `Option` the call fills.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                None,
            )?;
        }
        let d3d = d3d.ok_or_else(|| Error::new(E_FAIL, "no D3D device"))?;
        let dxgi: IDXGIDevice = d3d.cast()?;
        let factory: ID2D1Factory1 = graphics.d2d.cast()?;
        // SAFETY: Every interface here outlives the calls made on it, and the
        // print control is closed before it is dropped.
        // The device must outlive the context and the control it made, so it is
        // held by this binding until the job is closed below.
        let (_device, context, control) = unsafe {
            let device = factory.CreateDevice(&dxgi)?;
            let context = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let target = package_target(&printer, file, ticket.as_ref())?;
            let properties = D2D1_PRINT_CONTROL_PROPERTIES {
                fontSubset: D2D1_PRINT_FONT_SUBSET_MODE_DEFAULT,
                // 字は字のまま流れるので、この値が効くのは画像などラスタにする
                // ものだけ。
                rasterDPI: 300.0,
                colorSpace: D2D1_COLOR_SPACE_SRGB,
            };
            let control = device.CreatePrintControl(&graphics.wic, &target, Some(&properties))?;
            (device, context, control)
        };
        let size = D2D_SIZE_F {
            width: paper.width,
            height: paper.height,
        };
        for page in 0..pages {
            let list = draw_page(graphics, &context, engine, paper, page)?;
            // SAFETY: The command list is closed inside `draw_page` and lives
            // until the call returns.
            unsafe { control.AddPage(&list, size, None, None, None)? };
        }
        // SAFETY: Closing ends the job; the printer writes nothing until it is
        // called.
        unsafe { control.Close()? };
        Ok(pages)
    })
}

/// この紙に何字入り、何行入るか。
///
/// **書き手が見たいのはこの2つ**（書き手の問い 2026-09-16：「Widthはどこで設定
/// するか」）。余白を動かせば字数と行数が動く、という関係をその場で見せるために
/// ある——余白をmmで言われても、原稿が何字詰めになるかは分からない。
///
/// どちらも**本文の並の行**での数で、見出しや字下げのある行はこれより少ない。
pub fn page_grid(engine: &TextEngine, paper: Paper) -> (u32, u32) {
    let (page_flow, _) = paper.printable(engine.mode);
    let cell = engine.typography.cell_advance();
    let line_box = engine.fit.line_box(engine.margin, 0.0);
    let line = engine
        .plan
        .blocks
        .iter()
        .flat_map(|block| block.lines.iter())
        .map(|line| line.flow_size)
        .find(|size| *size > 0.0)
        .unwrap_or(cell);
    (
        (line_box / cell).floor().max(0.0) as u32,
        (page_flow / line).floor().max(0.0) as u32,
    )
}

/// この文書がこの紙で何枚になるか。
pub fn page_count(engine: &TextEngine, paper: Paper) -> usize {
    page_ranges(engine, paper).len()
}

/// 紙の切れ目を決める。返すのは、各ページが流れの軸で占める範囲。
///
/// **行の切れ目でしか切らない**（要件 7.10）。編集画面は続いた1枚として組んであり、
/// 紙にするときだけ切る——字の途中や行の途中で切れば、同じ行が2枚に割れて両方に
/// 半分ずつ出る。入らない行は、その行ごと次の紙へ送る。
///
/// `［＃改ページ］`（7.8）はここで効く。**紙が余っていても次の紙へ移る**のが、
/// 画面であの破線が言っていたことである。
///
/// **追い出しはしない**（要件 7.10：組版の細かさは追わない）。見出しが紙の終わりに
/// 一行だけ残ることはあるが、それを避ける仕掛けは紙のためだけの組版であり、
/// 画面と紙で違う原稿になる道の入口である。
fn page_ranges(engine: &TextEngine, paper: Paper) -> Vec<(f32, f32)> {
    let (page_flow, _) = paper.printable(engine.mode);
    let order = engine.plan.order;
    // 読む順に数えた位置。**縦書きは流れが負の側へ進む**ので、読み始めからの距離に
    // 直しておくと、どちらの組み方も「増えていく数」ひとつで扱える。
    let reading = |flow: f32, size: f32| match order {
        FlowOrder::Ascending => (flow, flow + size),
        FlowOrder::Descending => (-(flow + size), -flow),
    };
    let mut pages = Vec::new();
    let mut start = 0.0_f32;
    let mut reached = 0.0_f32;
    let mut opened = false;
    for (index, block) in engine.plan.blocks.iter().enumerate() {
        let breaks = engine.page_break_lines(index);
        if !breaks.is_empty() {
            eprintln!(
                "DEBUG block {index} breaks at {breaks:?} lines={}",
                block.lines.len()
            );
        }
        // 表には行の表が無い（`measure_table`）。**丸ごと一つの単位**として扱う
        // ——紙をまたぐ表は上から出て、はみ出したぶんは切り落とされる。
        let units: Vec<(f32, f32, bool)> = if block.grid.is_some() || block.lines.is_empty() {
            let (from, to) = reading(block.flow_start, block.flow_size);
            vec![(from, to, false)]
        } else {
            block
                .lines
                .iter()
                .map(|line| {
                    let (from, to) = reading(block.to_global_flow(line.flow_start), line.flow_size);
                    let breaks_after = breaks
                        .iter()
                        .any(|at| *at >= line.utf16_start && *at < line.utf16_end());
                    (from, to, breaks_after)
                })
                .collect()
        };
        for (from, to, breaks_after) in units {
            if !opened {
                start = from;
                opened = true;
            }
            // 入らなければ、この行ごと次の紙へ。
            if to - start > page_flow && from > start {
                pages.push((start, from));
                start = from;
            }
            reached = to;
            if breaks_after {
                pages.push((start, reached));
                opened = false;
            }
        }
    }
    if opened && reached > start {
        pages.push((start, reached));
    }
    // 読む順の範囲を、流れの軸の範囲へ戻す。
    pages
        .into_iter()
        .map(|(from, to)| match order {
            FlowOrder::Ascending => (from, to),
            FlowOrder::Descending => (-to, -from),
        })
        .collect()
}

/// 一枚ぶんの描画を、印刷の口が受け取る形（コマンドリスト）にして返す。
fn draw_page(
    graphics: &mut Graphics,
    context: &ID2D1DeviceContext,
    engine: &mut TextEngine,
    paper: Paper,
    page: usize,
) -> Result<ID2D1CommandList> {
    // SAFETY: The command list outlives the draw, the target is set and cleared
    // around it, and BeginDraw/EndDraw are paired.
    let list = unsafe {
        let list = context.CreateCommandList()?;
        context.SetTarget(&list);
        context.BeginDraw();
        list
    };
    let target: ID2D1RenderTarget = context.clone().into();
    draw_page_onto(graphics, &target, engine, paper, page, 1.0)?;
    // SAFETY: Paired with BeginDraw above; closing the list is what makes it
    // replayable by the print control.
    unsafe {
        context.EndDraw(None, None)?;
        context.SetTarget(None);
        list.Close()?;
    }
    Ok(list)
}

/// 一枚ぶんを、渡された面に描く。
///
/// **紙もプレビューもこれを呼ぶ**（要件 7.10）——プレビューに映る1枚と、プリンタへ
/// 送る1枚が同じ処理の同じ絵であるのは、この関数が1つだからである。面がコマンド
/// リストなら印刷へ、ビットマップなら画面へ行く。
///
/// そして**字を並べるのは画面のタイルと同じ処理**（[`super::draw_block`]）。紙の
/// 上のどこに置くかは面の変換で動かすので、並べる側は自分が紙に出ていることを
/// 知らない。
fn draw_page_onto(
    graphics: &mut Graphics,
    target: &ID2D1RenderTarget,
    engine: &mut TextEngine,
    paper: Paper,
    page: usize,
    scale: f32,
) -> Result<()> {
    let mode = engine.mode;
    let (_, page_cross) = paper.printable(mode);
    let ranges = page_ranges(engine, paper);
    let Some(&(low, high)) = ranges.get(page) else {
        return Ok(());
    };
    // 描き始めは紙の端に合わせる（[`page_flow_start`]）。**拾うのはこの紙に載る行
    // だけ**なので、範囲はそれとは別に、切れ目そのものを渡す。
    let flow_low = page_flow_start(engine, paper, page);
    let tiles = engine.plan.visible_tiles(
        -low,
        (high - low).max(1.0),
        engine.tile_flow_size(),
        0,
        CrossSlices {
            extent: engine.line_extent(),
            tile_size: engine.tile_cross_size(),
            viewport: 0.0,
            visible: page_cross,
        },
    );
    let tasks = page_tasks(engine, &tiles);
    let inks = Inks::on(target)?;
    // **この紙のぶんだけを見せる。**切れ目は行の切れ目に置いてあるが、ブロックは
    // 行より大きい単位なので、紙をまたぐブロックは丸ごと描かれる——次の紙が同じ
    // ブロックの続きを続きの位置から見せるので、こちらは自分のぶんで切る。
    //
    // **流れの軸は紙ではなくページの範囲で切る。**`［＃改ページ］`で切れた紙は
    // 中身が1ページぶんに満たず、そこを紙の端まで開けておくと、次の紙に出るはずの
    // 行がこの紙の余白に出る。
    let (clip_from, clip_to) = (low - flow_low, high - flow_low);
    let (left, top) = {
        let (x, y) = mode.to_screen(clip_from, 0.0);
        (paper.margin + x, paper.margin + y)
    };
    let (right, bottom) = {
        let (x, y) = mode.to_screen(clip_to, page_cross);
        (paper.margin + x, paper.margin + y)
    };
    let clip = D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    };
    // SAFETY: The clip is pushed in page coordinates and popped below, before
    // the target leaves this function.
    unsafe {
        target.SetTransform(&place(scale, 0.0, 0.0));
        target.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
    }
    for task in &tasks {
        inks.set(&task.typography, &task.words);
        // 紙の上でのこのタイルの左上。**タイルは自分の中を0から数えて描く**ので、
        // ここで動かすだけで置ける。
        let flow_offset = task.span.flow_start as f32 - flow_low;
        let cross_offset = task.span.cross_start as f32;
        let (dx, dy) = mode.to_screen(flow_offset, cross_offset);
        // SAFETY: The transform is set on the live target and put back below.
        unsafe {
            target.SetTransform(&place(scale, paper.margin + dx, paper.margin + dy));
        }
        let cached = match task.block.grid {
            Some(_) => None,
            None => engine.layout_for(graphics, task.span.block_index).ok(),
        };
        draw_block(graphics, target, &inks, task, cached)?;
    }
    // SAFETY: Paired with the push above.
    unsafe {
        target.SetTransform(&place(scale, 0.0, 0.0));
        target.PopAxisAlignedClip();
    }
    // **ノンブルは本文の外**（要件 7.10）。切り取る枠の外に出るので、閂を外して
    // から描く。
    let spec = tasks
        .first()
        .map(|task| task.typography.clone())
        .unwrap_or_else(|| std::sync::Arc::new(super::Typography::new(14.0)));
    draw_nombre(graphics, target, &inks, paper, page, ranges.len(), &spec)?;
    Ok(())
}

/// 何枚目かを、紙の下の余白に打つ。
///
/// **ページという概念は紙にしか無い**（要件 7.10）ので、画面には出ない数である。
/// 下の余白の中ほどに、本文より小さく、横書きで置く——縦書きの本でもノンブルは
/// 横に寝かせて読む。
fn draw_nombre(
    graphics: &mut Graphics,
    target: &ID2D1RenderTarget,
    inks: &Inks,
    paper: Paper,
    page: usize,
    pages: usize,
    spec: &super::Typography,
) -> Result<()> {
    if pages == 0 {
        return Ok(());
    }
    // 本文の7割。小さすぎると読めず、大きいと本文と競う。
    let size = (spec.font_size * 0.7).max(6.0);
    let spec = super::Typography {
        font_size: size,
        line_spacing: 1.0,
        ruby_room: false,
        ..spec.clone()
    };
    let format = graphics.text_format(&spec, WritingMode::Horizontal)?;
    let text: Vec<u16> = (page + 1).to_string().encode_utf16().collect();
    let band = D2D_RECT_F {
        left: paper.margin,
        // 余白の真ん中あたり。本文の下端からも紙の端からも離れる。
        top: paper.height - paper.margin * 0.72,
        right: paper.width - paper.margin,
        bottom: paper.height - paper.margin * 0.2,
    };
    // SAFETY: The format and brush outlive the call, and the text is a live
    // buffer for its length.
    unsafe {
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        target.DrawText(
            &text,
            &format,
            &band,
            &inks.brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        // **借りた書式は返す。**画面のタイルと同じ入れ物を使っているので、寄せ方を
        // 置いたままにすると本文が真ん中へ寄る。
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
    }
    Ok(())
}

/// この紙に載せるブロックたち。
///
/// 画面のタイルと同じものだが、**改ページの破線だけ落とす**（2026-09-16、書き手の
/// 判断）。あの破線は画面が紙を切らないから引いてあるもので、紙では実際にページが
/// 変わる——両方あると「消し忘れの線」に見える。
fn page_tasks(engine: &TextEngine, tiles: &[TileSpan]) -> Vec<super::TileTask> {
    let mut tasks = engine.tile_tasks(tiles, None);
    for task in &mut tasks {
        task.lines
            .retain(|run| run.ornament != LineOrnament::PageBreak);
    }
    tasks
}

/// 拡大と移動を1つにした変換。**拡大してから動かす**ので、渡す位置は紙の寸法で
/// 言える。
fn place(scale: f32, x: f32, y: f32) -> windows_numerics::Matrix3x2 {
    windows_numerics::Matrix3x2 {
        M11: scale,
        M12: 0.0,
        M21: 0.0,
        M22: scale,
        M31: x * scale,
        M32: y * scale,
    }
}

/// `page`枚目を、紙のどこから描き始めるか（流れの軸の座標）。
///
/// **読み始めの側の端に合わせる**——縦書きは紙の右、横書きは紙の上。最後の紙のように
/// 中身が1ページぶんに満たなくても、余りは読み終わりの側に出る。
fn page_flow_start(engine: &TextEngine, paper: Paper, page: usize) -> f32 {
    let (page_flow, _) = paper.printable(engine.mode);
    let ranges = page_ranges(engine, paper);
    let Some(&(low, high)) = ranges.get(page) else {
        return 0.0;
    };
    match engine.plan.order {
        FlowOrder::Ascending => low,
        FlowOrder::Descending => high - page_flow,
    }
}

/// 一枚を絵にする。**印刷プレビューが見るもの**（要件 7.10）で、プリンタへ送る
/// 1枚と同じ処理が描く。BGRAの画素と、その幅・高さを返す。
pub fn render_page(
    engine: &mut TextEngine,
    paper: Paper,
    page: usize,
    scale: f32,
) -> Result<(Vec<u8>, u32, u32)> {
    let width = (paper.width * scale).round().max(1.0) as u32;
    let height = (paper.height * scale).round().max(1.0) as u32;
    with_graphics(|graphics| {
        let (target, bitmap) = {
            let cache = graphics.render_target(width, height)?;
            (cache.target.clone(), cache.bitmap.clone())
        };
        // SAFETY: The target and bitmap live as long as the cache entry, and
        // BeginDraw/EndDraw are paired.
        unsafe {
            target.BeginDraw();
            // 紙は白い。**画面のアイボリーではない**——紙の色は紙が持っている。
            target.Clear(Some(&colour([1.0, 1.0, 1.0])));
        }
        draw_page_onto(graphics, &target, engine, paper, page, scale)?;
        // SAFETY: Paired with BeginDraw above.
        unsafe { target.EndDraw(None, None)? };
        let stride = width * 4;
        let mut pixels = vec![0u8; stride as usize * height as usize];
        let rect = WICRect {
            X: 0,
            Y: 0,
            Width: width as i32,
            Height: height as i32,
        };
        // SAFETY: The rectangle is the whole bitmap and the buffer matches the
        // stride and height asked for.
        unsafe {
            let source: IWICBitmapSource = bitmap.cast()?;
            source.CopyPixels(&rect, stride, &mut pixels)?;
        }
        Ok((pixels, width, height))
    })
}

/// 印刷の仕事の受け取り先。`file`があれば、プリンタはそこへ書く。
fn package_target(
    printer: &str,
    file: Option<&Path>,
    ticket: Option<&IStream>,
) -> Result<windows::Win32::Storage::Xps::Printing::IPrintDocumentPackageTarget> {
    let printer = HSTRING::from(printer);
    let job = HSTRING::from("10_Editor");
    // SAFETY: Both strings outlive the call, and the stream is either a live
    // file stream or nothing.
    unsafe {
        let factory: IPrintDocumentPackageTargetFactory = CoCreateInstance(
            &PrintDocumentPackageTargetFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )?;
        let stream: Option<IStream> = match file {
            Some(path) => {
                let path = HSTRING::from(path.as_os_str());
                Some(SHCreateStreamOnFileEx(
                    PCWSTR(path.as_ptr()),
                    (STGM_CREATE.0 | STGM_READWRITE.0) as u32,
                    0,
                    true,
                    None,
                )?)
            }
            None => None,
        };
        factory.CreateDocumentPackageTargetForPrintJob(
            PCWSTR(printer.as_ptr()),
            PCWSTR(job.as_ptr()),
            stream.as_ref(),
            ticket,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{PreviewDocument, Reading, line_styles_reading};
    use crate::text_blocks::StyledText;
    use crate::{directwrite_render::LineFit, text_blocks::Typography};

    /// 紙の寸法で組んだ組版器を1つ。**行の体裁は原稿から読む**——プレビューの本文は
    /// 行頭の注記（`［＃地付き］`）を既に取り除いてある。
    fn engine_on_paper(source: &str, mode: WritingMode, paper: Paper) -> TextEngine {
        let preview = PreviewDocument::from_source(source);
        let styles = line_styles_reading(source, Reading::all());
        let styled = StyledText::marked(&preview.text, &styles, preview.marks())
            .with_markers(preview.markers());
        let (_, cross) = paper.printable(mode);
        let mut engine = TextEngine::new(mode);
        engine
            .update(
                styled,
                LineFit::Extent(cross.round() as u32),
                &Typography::new(14.0),
            )
            .expect("lay the document out at the paper's size");
        engine
    }

    fn long_document(paragraphs: usize) -> String {
        (0..paragraphs)
            .map(|at| format!("{at}段落目です。これは紙を何枚も要る長さにするための本文です。\n\n"))
            .collect()
    }

    /// 要件 7.10: `［＃改ページ］`はそこで紙を改める。**紙が余っていても**である。
    #[test]
    fn a_page_break_note_ends_the_page() {
        for mode in [WritingMode::Vertical, WritingMode::Horizontal] {
            let paper = Paper::default();
            let (page_flow, _) = paper.printable(mode);
            let source = "はじめの行です。\n\n［＃改ページ］\n\n次の紙に出る行です。\n";
            let engine = engine_on_paper(source, mode, paper);
            let pages = page_ranges(&engine, paper);
            assert_eq!(pages.len(), 2, "the note must cut the paper: {mode:?}");
            let first = pages[0].1 - pages[0].0;
            assert!(
                first < page_flow * 0.5,
                "the first sheet is cut long before it is full: {first} of {page_flow} ({mode:?})"
            );
        }
    }

    /// 紙より長いページは作らない。**行の切れ目でしか切らない**ので、ぴったりには
    /// ならないが、はみ出しはしない。
    #[test]
    fn no_page_holds_more_than_the_paper() {
        for mode in [WritingMode::Vertical, WritingMode::Horizontal] {
            let paper = Paper::default();
            let (page_flow, _) = paper.printable(mode);
            let engine = engine_on_paper(&long_document(60), mode, paper);
            let pages = page_ranges(&engine, paper);
            assert!(pages.len() > 2, "the sample must need several sheets");
            for (at, (low, high)) in pages.iter().enumerate() {
                assert!(
                    high - low <= page_flow + 0.5,
                    "page {at} is longer than the paper: {} of {page_flow} ({mode:?})",
                    high - low
                );
            }
        }
    }

    /// 要件 7.10: 紙では改ページの破線を引かない。**画面では引く**——画面は紙を
    /// 切らないので、あの線だけが切れ目を言っている。
    #[test]
    fn the_page_break_dashes_stay_on_the_screen() {
        let paper = Paper::default();
        let source = "はじめの行です。\n\n［＃改ページ］\n\n次の紙に出る行です。\n";
        let engine = engine_on_paper(source, WritingMode::Vertical, paper);
        // 文書の端から端まで。**縦書きの流れは負の側へ進む**ので、見ている位置は
        // その端を裏返した値で言う（`visible_flow_range`）。
        let (low, high) = engine.plan.flow_bounds();
        let tiles = engine.visible_tiles(-low, high - low, 0, 0.0, 4000.0);
        let on_screen = engine.tile_tasks(&tiles, None);
        assert!(
            on_screen.iter().any(|task| task
                .lines
                .iter()
                .any(|run| run.ornament == LineOrnament::PageBreak)),
            "the screen draws the dashes"
        );
        let on_paper = page_tasks(&engine, &tiles);
        assert!(
            on_paper.iter().all(|task| task
                .lines
                .iter()
                .all(|run| run.ornament != LineOrnament::PageBreak)),
            "the paper does not"
        );
    }

    /// 要件 7.10: ノンブルは**下の余白**に、紙の真ん中で打つ。本文の枠の中には
    /// 入らない——入れば1行ぶん本文が減る。
    #[test]
    fn the_page_number_stands_in_the_bottom_margin() {
        let paper = Paper::default();
        let mut engine = engine_on_paper(&long_document(40), WritingMode::Vertical, paper);
        let (pixels, width, height) =
            render_page(&mut engine, paper, 1, 1.0).expect("draw the page");
        let dark = |x: u32, y: u32| {
            let at = ((y * width + x) * 4) as usize;
            pixels.get(at).is_some_and(|blue| *blue < 120)
        };
        let band = |from: f32, to: f32| {
            let (from, to) = (from.round() as u32, (to.round() as u32).min(height));
            (from..to)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .filter(|(x, y)| dark(*x, *y))
                .count()
        };
        // 本文が終わったところから紙の端までに、数が1つ立っている。
        let margin = paper.margin;
        assert!(
            band(paper.height - margin, paper.height) > 0,
            "the page number must be printed below the text"
        );
        // そして本文の枠には食い込まない。
        assert_eq!(
            band(paper.height - margin - 4.0, paper.height - margin),
            0,
            "and must not reach into the text area"
        );
    }

    /// 紙と紙のあいだで本文が消えたり、二度出たりしない。**前の紙が終わったところ
    /// から次の紙が始まる。**
    #[test]
    fn the_sheets_join_without_a_gap_or_an_overlap() {
        for mode in [WritingMode::Vertical, WritingMode::Horizontal] {
            let paper = Paper::default();
            let engine = engine_on_paper(&long_document(40), mode, paper);
            let pages = page_ranges(&engine, paper);
            // **並びは読む順**だが、範囲そのものは流れの軸で言ってある。縦書きは
            // 読むほど小さい側へ進むので、前の紙の「下」が次の紙の「上」に当たる。
            for pair in pages.windows(2) {
                let (before, after) = (pair[0], pair[1]);
                let (end, start) = match engine.plan.order {
                    FlowOrder::Ascending => (before.1, after.0),
                    FlowOrder::Descending => (before.0, after.1),
                };
                assert!(
                    (end - start).abs() < 0.5,
                    "the sheets do not meet: {end} against {start} ({mode:?})"
                );
            }
        }
    }
}

/// プリンタの答え：どこへ、どの紙で、どの設定で刷るか。
///
/// **用紙・向き・部数はWindowsに訊く**（要件 7.10）。訊いた結果はそのまま印刷の
/// 仕組みへ渡す「印刷チケット」になり、紙の大きさだけこちらも読む——プレビューが
/// 実際の紙を映すためである。
pub struct Chosen {
    pub printer: String,
    pub paper: Paper,
    /// Windowsの設定をそのまま包んだもの。プリンタへ渡す。
    ticket: Option<IStream>,
}

impl Chosen {
    /// 紙の大きさだけ知りたいとき（プレビューを開くとき）。
    pub fn paper(&self) -> Paper {
        self.paper
    }
}

/// 既定のプリンタを、何も訊かずに読む。
///
/// **プレビューは実際の紙を映す**（要件 7.10）ので、開く前にここを通る。プリンタが
/// 1台も無ければ`None`で、そのときは[`Paper::default`]（A4縦）を使う。
pub fn default_printer() -> Option<Chosen> {
    // SAFETY: The dialog is asked not to show itself, so nothing here touches
    // the screen; every handle it fills is freed in `chosen_from`.
    unsafe { ask_windows(HWND::default(), PD_RETURNDEFAULT) }
}

/// Windowsのプリンタ選択を出す。書き手が取り消せば`None`。
pub fn ask(owner: HWND) -> Option<Chosen> {
    // SAFETY: As above, and the owner window outlives the modal dialog.
    unsafe { ask_windows(owner, PRINTDLGEX_FLAGS(0)) }
}

/// 余白はこの編集器が持つので、Windowsからは**紙の大きさだけ**受け取る。
unsafe fn ask_windows(owner: HWND, extra: PRINTDLGEX_FLAGS) -> Option<Chosen> {
    let mut dialog = PRINTDLGW {
        lStructSize: size_of::<PRINTDLGW>() as u32,
        hwndOwner: owner,
        // `PD_RETURNIC`は測るためだけの軽い手がかり（絵は描かない）。紙の大きさを
        // 訊くのに要る。ページ番号と選択範囲は、こちらが紙を切るので出さない。
        Flags: PRINTDLGEX_FLAGS(
            PD_RETURNIC.0 | PD_NOPAGENUMS.0 | PD_NOSELECTION.0 | PD_USEDEVMODECOPIESANDCOLLATE.0,
        ) | extra,
        nCopies: 1,
        ..Default::default()
    };
    // SAFETY: The struct is filled in above and every handle it comes back with
    // is released below.
    unsafe {
        if !PrintDlgW(&mut dialog).as_bool() {
            return None;
        }
    }
    // SAFETY: Both handles are the dialog's own and are unlocked and freed here.
    let chosen = unsafe {
        let printer = device_name(dialog.hDevNames);
        let devmode = GlobalLock(dialog.hDevMode) as *const DEVMODEW;
        let paper = paper_of(dialog.hDC).unwrap_or_default();
        let ticket = printer
            .as_ref()
            .and_then(|name| ticket_of(name, devmode).ok());
        if !devmode.is_null() {
            let _ = GlobalUnlock(dialog.hDevMode);
        }
        printer.map(|printer| Chosen {
            printer,
            paper,
            ticket,
        })
    };
    // SAFETY: Each handle is freed once, and the information context is a DC.
    unsafe {
        if !dialog.hDevMode.is_invalid() {
            let _ = GlobalFree(Some(dialog.hDevMode));
        }
        if !dialog.hDevNames.is_invalid() {
            let _ = GlobalFree(Some(dialog.hDevNames));
        }
        if !dialog.hDC.is_invalid() {
            let _ = DeleteDC(dialog.hDC);
        }
    }
    chosen
}

/// `DEVNAMES`の中のプリンタ名。**名前の並びの中の位置**で入っている。
unsafe fn device_name(handle: HGLOBAL) -> Option<String> {
    if handle.is_invalid() {
        return None;
    }
    // SAFETY: The handle is the dialog's, and the block is unlocked before
    // returning.
    unsafe {
        let names = GlobalLock(handle) as *const DEVNAMES;
        if names.is_null() {
            return None;
        }
        let base = names as *const u16;
        let at = base.add((*names).wDeviceOffset as usize);
        let name = PCWSTR(at).to_string().ok().filter(|name| !name.is_empty());
        let _ = GlobalUnlock(handle);
        name
    }
}

/// このプリンタが送る紙の大きさ（DIP）。
///
/// **端から端まで**（`PHYSICAL…`）を訊く。印字できる範囲ではなく紙そのもので、
/// 余白はこちらが持っているからである。
unsafe fn paper_of(context: HDC) -> Option<Paper> {
    if context.is_invalid() {
        return None;
    }
    // SAFETY: The context is the dialog's information context, alive until the
    // caller deletes it.
    unsafe {
        let (dots_x, dots_y) = (
            GetDeviceCaps(Some(context), LOGPIXELSX),
            GetDeviceCaps(Some(context), LOGPIXELSY),
        );
        let (wide, tall) = (
            GetDeviceCaps(Some(context), PHYSICALWIDTH),
            GetDeviceCaps(Some(context), PHYSICALHEIGHT),
        );
        if dots_x <= 0 || dots_y <= 0 || wide <= 0 || tall <= 0 {
            return None;
        }
        Some(Paper {
            width: wide as f32 * 96.0 / dots_x as f32,
            height: tall as f32 * 96.0 / dots_y as f32,
            ..Paper::default()
        })
    }
}

/// Windowsの設定（`DEVMODE`）を、印刷の仕組みが読む形（印刷チケット）に直す。
unsafe fn ticket_of(printer: &str, devmode: *const DEVMODEW) -> Result<IStream> {
    if devmode.is_null() {
        return Err(Error::new(E_FAIL, "no printer settings"));
    }
    let name = HSTRING::from(printer);
    // SAFETY: The provider is closed below, the stream outlives the call, and
    // the settings block is the size it says it is.
    unsafe {
        let provider = PTOpenProvider(PCWSTR(name.as_ptr()), 1)?;
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let size = u32::from((*devmode).dmSize) + u32::from((*devmode).dmDriverExtra);
        let converted = PTConvertDevModeToPrintTicket(
            provider,
            size,
            devmode as *const DEVMODEA,
            kPTJobScope,
            &stream,
        );
        let _ = PTCloseProvider(provider);
        converted?;
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        Ok(stream)
    }
}
