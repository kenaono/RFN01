//! 要件 7.10: 紙の形で見る——印刷。
//!
//! **実際に刷ってしまう試験**なので、いつもの試験からは外してある
//! （`--ignored`で名指ししたときだけ走る）。Windowsの印刷へ本当に仕事を出し、
//! 「Microsoft Print to PDF」に書かせたPDFを置く——書き手も私も、それを開いて
//! 目で確かめるためにこれを走らせる。
//!
//! ```text
//! EDITOR_PRINT_OUT=target\print WSLENV=EDITOR_PRINT_OUT \
//!   cargo.exe test --offline -- --ignored prints_
//! ```
use super::*;
use directwrite_render::print::{Destination, PDF_PRINTER, Paper, page_count};
use std::path::PathBuf;

/// 刷ったものの置き場。言われていなければ`target\print`。
fn out_dir() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var("EDITOR_PRINT_OUT").unwrap_or_else(|_| "target\\print".to_owned()),
    );
    std::fs::create_dir_all(&dir).expect("output directory");
    dir
}

/// 紙の寸法で組んだ組版器を1つ。
///
/// **紙の行の長さで組む**のが肝で、画面の幅で組んだものをそのまま紙へ出せば、
/// 折り返しが紙に合わない（要件 7.10）。
fn engine_on_paper(source: &str, mode: WritingMode, paper: Paper) -> TextEngine {
    let preview = document::PreviewDocument::from_source(source);
    // **行の体裁は原稿から読む。**プレビューの本文は行頭の注記（`［＃地付き］`）を
    // 既に取り除いてあるので、そちらから数えると地付きが消える（画面もそうしている）。
    let styles = document::line_styles_reading(source, document::Reading::all());
    let styled =
        StyledText::marked(&preview.text, &styles, preview.marks()).with_markers(preview.markers());
    let typography = Typography {
        // 紙は白、字は黒。画面の紙はアイボリーだが、**紙の色は紙が持っている**。
        paper: [1.0, 1.0, 1.0],
        ink: [0.0, 0.0, 0.0],
        paper_painted: false,
        line_numbers: false,
        ..Typography::new(14.0)
    };
    let (_, cross) = paper.printable(mode);
    let mut engine = TextEngine::new(mode);
    engine
        .update(
            styled,
            directwrite_render::LineFit::Extent(cross.round() as u32),
            &typography,
        )
        .expect("lay the document out at the paper's size");
    engine
}

/// 縦書きの見本を1つ、PDFにする。**開いて目で見るためのもの。**
#[test]
#[ignore = "Windowsの印刷へ本当に仕事を出す"]
fn prints_a_vertical_sample_to_pdf() {
    print_sample(WritingMode::Vertical, "vertical.pdf");
}

/// 同じものを横書きで。縦と並べれば、どちらかにだけ出る崩れが分かる。
#[test]
#[ignore = "Windowsの印刷へ本当に仕事を出す"]
fn prints_a_horizontal_sample_to_pdf() {
    print_sample(WritingMode::Horizontal, "horizontal.pdf");
}

fn print_sample(mode: WritingMode, name: &str) {
    print_file("testdata/13_ルビ・傍点・縦中横.md", mode, name);
}

fn print_file(from: &str, mode: WritingMode, name: &str) {
    let source = std::fs::read_to_string(from).expect("the sample document");
    let paper = Paper::default();
    let mut engine = engine_on_paper(&source, mode, paper);
    let pages = page_count(&engine, paper);
    assert!(pages > 0, "the sample must fill at least one page");
    let path = out_dir().join(name);
    let _ = std::fs::remove_file(&path);
    let printed = directwrite_render::print::print(
        &mut engine,
        paper,
        Destination::PdfFile(&path),
        &directwrite_render::print::Trim::standing(),
    )
    .unwrap_or_else(|error| {
        panic!(
            "printing to {PDF_PRINTER} failed: {error}\n\
                 （このプリンタはWindowsの機能なので、切ってあれば無い）"
        )
    });
    assert_eq!(printed, pages, "every page must reach the printer");
    let written = std::fs::metadata(&path)
        .unwrap_or_else(|error| panic!("{} was not written: {error}", path.display()))
        .len();
    assert!(written > 0, "{} is empty", path.display());
    println!(
        "{mode:?}: {printed} pages, {written} bytes at {}",
        path.display()
    );
}

/// 折り返しを入れていない原稿を刷る。**縦書きの段がどこまで伸びるか**を見る
/// ためのもので、原稿の側で短く折り返してあれば段もそこで終わる。
#[test]
#[ignore = "Windowsの印刷へ本当に仕事を出す"]
fn prints_a_document_without_hard_wraps() {
    print_file(
        "testdata/09_段落長の計測.md",
        WritingMode::Vertical,
        "long-lines.pdf",
    );
}

/// 紙を絵にして置く。**私と書き手が目で見るためのもの**で、プリンタへ送る1枚と
/// 同じ処理が描いている。`EDITOR_PRINT_PAGES`で対象を変えられる
/// （`<ファイル>:<縦か横>:<何枚目まで>`）。
#[test]
#[ignore = "紙の絵を置く"]
fn draws_pages_as_pictures() {
    let want = std::env::var("EDITOR_PRINT_PAGES")
        .unwrap_or_else(|_| "testdata/13_ルビ・傍点・縦中横.md:v:3".to_owned());
    let mut parts = want.split(':');
    let file = parts.next().unwrap_or_default();
    let mode = match parts.next() {
        Some("h") => WritingMode::Horizontal,
        _ => WritingMode::Vertical,
    };
    let limit: usize = parts.next().and_then(|at| at.parse().ok()).unwrap_or(3);
    let source = std::fs::read_to_string(file).expect("the sample document");
    let paper = Paper::default();
    let mut engine = engine_on_paper(&source, mode, paper);
    for page in 0..limit.min(page_count(&engine, paper)) {
        let (pixels, width, height) = directwrite_render::print::render_page(
            &mut engine,
            paper,
            page,
            2.0,
            &directwrite_render::print::Trim::standing(),
        )
        .expect("draw the page");
        let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
        for bgra in pixels.chunks_exact(4) {
            ppm.extend([bgra[2], bgra[1], bgra[0]]);
        }
        let name = format!("{mode:?}-{}.ppm", page + 1).to_lowercase();
        std::fs::write(out_dir().join(&name), ppm).expect("write the page picture");
        println!("{name}: {width}x{height}");
    }
}
