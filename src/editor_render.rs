//! Shared text layout and tile rendering. Hosts supply values and publish results.
use super::*;

pub(crate) struct LayoutOptions {
    pub reading: document::Reading,
    pub preview: bool,
    pub viewer: bool,
    pub vertical: bool,
    pub zoom: i32,
    pub scroll: f32,
    pub viewport: f32,
    pub words: std::sync::Arc<word_marks::WordMarks>,
    pub find_showing: bool,
    pub needle: String,
    pub rules: find::Rules,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn layout(
    graphics: &mut PaneGraphics,
    view: &mut PaneView,
    document: &OpenDocument,
    source: &str,
    line_fit: LineFit,
    typography: &Typography,
    active_line_start: Option<usize>,
    caret_source_byte: Option<usize>,
    selection: PaneSelection,
    preedit: &str,
    options: &LayoutOptions,
) -> windows::core::Result<PaneLayout> {
    let mut counts = document.counts.borrow_mut();
    let styles = counts.get(source, options.reading).line_styles();

    let preview_started = Instant::now();
    // Which text this pane lays out — the whole of the third and fourth modes
    // (3.12). Everything below is written against `PaneText` and does not ask
    // which one it got.
    let slot = &mut view.preview_slot;
    let shown = if options.preview {
        let file = document.file.borrow();
        PaneText::Preview(slot.get(
            source,
            if options.viewer {
                None
            } else {
                active_line_start
            },
            options.reading,
            options.zoom,
            file.path().and_then(Path::parent),
        ))
    } else {
        PaneText::Source(source)
    };
    let differences = document.differences();
    let note = differences
        .as_ref()
        .map(|diff| {
            let target = if document.read_only() {
                pick("編集中の本文と比較", "Compared with the text being edited")
            } else {
                pick(
                    "外部版（取得時）と比較",
                    "Compared with the outside version (when read)",
                )
            };
            let grouped = if diff.grouped {
                pick("・広い変更をまとめて表示", " (wide changes shown together)")
            } else {
                ""
            };
            say!(
                "{target}：差分{}箇所{grouped}",
                "{target}: {} differences{grouped}",
                diff.ranges.len()
            )
        })
        .unwrap_or_default();
    let difference_utf16 = if preedit.is_empty() {
        differences
            .as_ref()
            .map(|diff| {
                diff.ranges
                    .iter()
                    .map(|range| {
                        let start = shown.utf16_at_source_byte(range.start) as u32;
                        let end = shown.utf16_at_source_byte(range.end) as u32;
                        (start, end.saturating_sub(start))
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let caret = caret_source_byte.map(|byte| shown.utf16_at_source_byte(byte) as u32);
    // 要件 8.5: the place this pane is holding its view on, if it still is.
    // **The caret having moved is the writer saying where to look**, and that
    // is the end of the hold — one comparison here rather than a flag every
    // caret-moving path would have to remember to clear.
    let anchor_utf16 = held_view(view.top_anchor, caret_source_byte)
        .map(|byte| shown.utf16_at_source_byte(byte) as u32);
    let selection_utf16 = selection.ends.map(|(start, end)| {
        (
            shown.utf16_at_source_byte(start) as u32,
            shown.utf16_at_source_byte(end) as u32,
        )
    });
    // E1: **探している語のありか**を、組んだ本文の位置へ写す。空の欄では
    // 一周も歩かない——これは打鍵のたびに通る道である。
    // **光るのは帯が出ているあいだだけ**（書き手の報告 2026-09-09：「その色を
    // 解除できません」）。語は面に残り続けるので（F3のため）、語があるかぎり
    // 光らせると、探し終えた紙が色を持ったままになる。**`Esc`で帯を閉じれば
    // 消える**——閉じる鍵が消す鍵でもある、というのがいちばん短い説明になる。
    let showing = options.find_showing;
    let needle = if showing { options.needle.as_str() } else { "" };
    let rules = options.rules;
    let scope = rules
        .within
        .filter(|_| showing)
        .map(|(start, end)| {
            let start = shown.utf16_at_source_byte(start) as u32;
            let end = shown.utf16_at_source_byte(end) as u32;
            (start, end.saturating_sub(start))
        })
        .filter(|(_, length)| *length > 0);
    let matches = if needle.is_empty() {
        Vec::new()
    } else {
        find::Search::new(needle, rules)
            .map(|search| search.spans(source, MAX_SHOWN_MATCHES))
            .unwrap_or_default()
            .into_iter()
            // **いま選ばれている一致には敷かない。**そこは選択が濃く出して
            // いるので、下に薄いのを重ねると同じ語なのに3段の濃さになる
            // （書き手の報告 2026-09-09：「色の変わり方が壊れています」）。
            .filter(|span| Some(*span) != selection.ends)
            .map(|(start, end)| {
                let start = shown.utf16_at_source_byte(start) as u32;
                let end = shown.utf16_at_source_byte(end) as u32;
                // **`selection_rects`が取るのは「始まりと長さ」**であって
                // 「始まりと終わり」ではない（書き手の報告 2026-09-09、
                // `検索.png`：語ではなく行が塗られていた。終わりを長さとして
                // 渡していたので、7文字目の2文字が「7文字目から9文字ぶん」に
                // なっていた）。選択の走りも同じ形で持っている。
                (start, end.saturating_sub(start))
            })
            // 組んだ本文で幅を持たないものは色を付けない——プレビューでは
            // ルビの読みのように**隠れている字**があり、そこに入った一致は
            // 写した先で長さ0になる。
            .filter(|(_, length)| *length > 0)
            .collect()
    };
    let (render_text, render_caret, preedit_range) = text_with_preedit(&shown, caret, preedit);
    let preview_ms = elapsed_ms(preview_started);

    let layout_started = Instant::now();
    let engine = &mut graphics.engine;
    // **下書きが乗っている行の印だけを外す**（書き手の報告 2026-09-12：
    // 「横書きで作業すると、IMEをON/OFFするたびに全体が上下に揺れます。
    // 1行くらい揺れる」）。
    //
    // 印は太字と斜体で、**字の幅が変わる＝折り返しが変わる**。ここは変換が
    // 立っているあいだ**文書じゅうの印を外して**いたので、変換のたびに印のある
    // ブロックが全部組み直され、**文書の高さが1〜2行ぶん変わって、下にある本文が
    // 丸ごと動いていた**（記録：`measured=48`と`content=17481↔17552`が交互に出る）。
    //
    // ずれるのは**下書きが挿さった行の、挿さった場所より後ろ**だけである。
    // 外すのもその1行でよく、**印の無い行に下書きを入れるのなら1行も外さない**
    // ——ほとんどの打鍵はこちらで、組み直しは起きない。
    let masked;
    let marks = match caret.filter(|_| !preedit.is_empty()) {
        None => shown.marks(),
        Some(caret) => {
            let at = shown.shown_byte_at_utf16(caret as usize);
            // 印は論理行ごとに並んでいる（`StyledText::spans`）ので、数えるのは
            // 挿さった場所より前の改行である。**数えるのは下書きを入れる前の
            // 本文**——入れたあとの`render_text`でも同じ数になるが、下書きに
            // 改行は無いという当てにしなくてよい。
            let line = shown.text()[..at].matches('\n').count();
            match shown.marks().get(line) {
                Some(spans) if !spans.is_empty() => {
                    masked = marks_without_line(shown.marks(), line);
                    masked.as_slice()
                }
                _ => shown.marks(),
            }
        }
    };
    // **The boxes are not held back the way the marks are.** A composition sits
    // at the caret, the caret's line is the active one, and the active line has
    // no box over its marker (要件 7.3.1) — so there is no range here for a
    // preedit to move. Holding them back would instead make the text jump by an
    // indent for as long as somebody is converting.
    let marked = StyledText::marked(&render_text, styles, marks);
    let styled = marked
        .with_markers(shown.markers())
        .with_source_line(shown.source_line());
    // 要件 7.9: **この面のモードの語を渡す**（`set_words`）。モードは文書ごと
    // なので、2つのペインが別の作品を開いていれば別の色分けになる。
    // **組み直しの判定には入らない**——幾何を1画素も動かさないので、変わっても
    // タイルだけが古くなる（技術検証 9.3.1）。
    engine.set_words(options.words.clone());
    // 追加要件 2026-09-15: 画像の行に描く絵。これも幾何の外（大きさは箱が持つ）。
    engine.set_pictures(shown.pictures());
    let through = engine
        .viewport_end_utf16(options.scroll, options.viewport)
        .max(render_caret.unwrap_or(0))
        .max(anchor_utf16.unwrap_or(0));
    let measured = engine.update_interactive(styled, line_fit, typography, through)?;
    let layout_ms = elapsed_ms(layout_started);

    // **Cut into runs only now.** A rectangle's runs are one per *layout* line,
    // and which lines those are is what the update just decided (要件 7.1).
    let runs = match selection_utf16 {
        Some((start, end)) if selection.rectangular => {
            rectangular_runs(options.vertical, engine, start, end)
        }
        Some((start, end)) if start < end => vec![(start, end - start)],
        _ => Vec::new(),
    };
    let selection_source = runs
        .iter()
        .map(|(start, length)| {
            (
                shown.source_byte_at_utf16(*start as usize),
                shown.source_byte_at_utf16((*start + *length) as usize),
            )
        })
        .collect::<Vec<_>>();

    view.caret_utf16 = render_caret;
    view.selection_utf16 = runs.clone();
    view.difference_utf16 = difference_utf16;
    view.selection_source = selection_source.clone();
    view.preedit_range = preedit_range;

    Ok(PaneLayout {
        comparison_note: note,
        render_caret,
        anchor_utf16,
        selection: runs,
        selection_source,
        matches,
        scope,
        measured,
        preview_ms,
        layout_ms,
    })
}

pub(crate) fn rectangular_runs(
    vertical: bool,
    engine: &mut TextEngine,
    start: u32,
    end: u32,
) -> Vec<(u32, u32)> {
    let (Ok(from), Ok(to)) = (engine.caret_geometry(start), engine.caret_geometry(end)) else {
        return Vec::new();
    };
    let lo = if vertical { from.y } else { from.x };
    let hi = if vertical { to.y } else { to.x };
    engine
        .rectangle_runs(start, end, lo, hi)
        .unwrap_or_default()
        .into_iter()
        .map(|(start, end)| (start, end - start))
        .collect()
}

pub(crate) struct TileViewport {
    pub scroll: f32,
    pub shown_flow: f32,
    pub scroll_across: f32,
    pub shown_across: f32,
    pub vertical: bool,
    pub shift: f32,
}

pub(crate) type TileCounts = (usize, usize, usize, usize, usize);

pub(crate) fn tiles(
    graphics: &mut PaneGraphics,
    preedit: Option<(u32, u32)>,
    viewport: TileViewport,
    prefetch: u32,
) -> windows::core::Result<(Vec<PreviewTile>, TileCounts)> {
    let TileViewport {
        scroll,
        shown_flow,
        scroll_across,
        shown_across,
        vertical,
        shift,
    } = viewport;
    // Taken apart so the engine and the images can be held at once: reaching
    // through `self` for each of them would borrow the whole cache.
    let PaneGraphics {
        engine,
        tiles: images,
        spare,
        uploaded_bytes,
    } = graphics;
    if engine.total_flow_size() == 0 {
        return Ok((Vec::new(), (0, 0, 0, 0, 0)));
    }
    // Tiles are cut out of the blocks the viewport crosses. Their size along
    // the flow tracks the pane's extent across it, so a taller window makes
    // tiles narrower rather than making each one costlier to rasterize.
    let desired = engine.visible_tiles(scroll, shown_flow, prefetch, scroll_across, shown_across);

    // Keyed by the fingerprint, never by position. The fingerprint names one
    // block's text at one slice of it, so a tile the layout moved is found
    // again unchanged, and two blocks with the same text share one image.
    let keyed = desired
        .iter()
        .map(|span| (*span, engine.tile_signature(*span, preedit)))
        .collect::<Vec<_>>();
    let mut missing: Vec<(TileSpan, u64)> = Vec::new();
    for (span, signature) in &keyed {
        let already =
            images.contains_key(signature) || missing.iter().any(|(_, queued)| queued == signature);
        if !already {
            missing.push((*span, *signature));
        }
    }
    let rendered = missing.len();
    let mut reused = 0_usize;

    if !missing.is_empty() {
        let spans = missing.iter().map(|(span, _)| *span).collect::<Vec<_>>();
        let mut drawn = TileImages {
            spare,
            drawing: None,
            produced: Vec::with_capacity(spans.len()),
            uploaded: 0,
            reused: 0,
        };
        engine.render_tiles(&spans, preedit, &mut drawn)?;
        *uploaded_bytes += drawn.uploaded;
        reused = drawn.reused;
        for (span, image, pixels) in drawn.produced {
            let queued = missing.iter().find(|(other, _)| *other == span);
            let Some((_, signature)) = queued else {
                continue;
            };
            images.insert(
                *signature,
                CachedTile {
                    image,
                    pixels,
                    last_flow: span.flow_start,
                },
            );
        }
    }

    // Placement is not cached, so a tile that slid along with the document's
    // growing edge costs one property assignment rather than a rasterization.
    let tiles = keyed
        .iter()
        .filter_map(|(span, signature)| {
            let cached = images.get_mut(signature)?;
            cached.last_flow = span.flow_start;
            Some(tile(vertical, shift, *span, cached.image.clone()))
        })
        .collect::<Vec<_>>();

    // **What is evicted leaves its buffer behind, for the refresh after
    // this one.** Not for this one: the pane is still showing the tiles of
    // the last refresh, so their images are alive until `set_tiles` below
    // replaces them — drawn into now, a buffer would be copied rather than
    // reused (Slint's `SharedVector::detach`), which is what allocating one
    // cost in the first place (技術検証 7.8).
    let viewport_center = -scroll + shown_flow * 0.5;
    let wanted = keyed
        .iter()
        .map(|(_, signature)| *signature)
        .collect::<Vec<_>>();
    evict_distant_tiles(images, spare, &wanted, viewport_center);
    let spare_held = spare.len();

    let tile_count = tiles.len();
    // **What was asked for, beside what was placed.** A tile whose image is
    // not in the cache when the placement runs is dropped without a word
    // (`images.get_mut`), and a pane missing one shows paper where its text
    // should be — which looks exactly like a document that has not been
    // drawn yet. The two numbers differing is the only sign from outside.
    Ok((
        tiles,
        (tile_count, keyed.len(), rendered, reused, spare_held),
    ))
}

pub(crate) fn tile(vertical: bool, shift: f32, span: TileSpan, source: Image) -> PreviewTile {
    let flow_start = span.flow_start + shift as i32;
    let flow_size = span.flow_size as i32;
    // 要件 9: where this slice sits across the page. A page that fits its
    // pane is one slice starting at nothing, which is what every tile was.
    let cross_start = span.cross_start as i32;
    let cross_size = span.cross_size as i32;
    if vertical {
        PreviewTile {
            x: flow_start,
            y: cross_start,
            width: flow_size,
            height: cross_size,
            source,
        }
    } else {
        PreviewTile {
            x: cross_start,
            y: flow_start,
            width: cross_size,
            height: flow_size,
            source,
        }
    }
}
