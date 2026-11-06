use super::*;
use crate::terminal_ui_tests::Harness;

const EDIT_MARKER: &str = "RFN_PERF_EDIT_HERE";
const EDITS: usize = 20;
const WAIT_LIMIT: Duration = Duration::from_secs(120);

fn ms(sample: Duration) -> f64 {
    sample.as_secs_f64() * 1000.0
}

pub(crate) fn report(case: &str, metric: &str, samples: &mut [Duration]) {
    if samples.is_empty() {
        return;
    }
    samples.sort_unstable();
    let n = samples.len();
    let median = (ms(samples[(n - 1) / 2]) + ms(samples[n / 2])) / 2.0;
    let p95 = ms(samples[(n * 95).div_ceil(100) - 1]);
    let max = ms(samples[n - 1]);
    eprintln!(
        "perf case={case} metric={metric} n={n} median_ms={median:.3} p95_ms={p95:.3} max_ms={max:.3}"
    );
}

#[test]
#[ignore = "Windows performance measurement; generate fixtures and run in Release"]
fn windows_performance_editor_many_paragraphs() {
    measure_editor("editor_many_paragraphs", "many-paragraphs.md");
}

#[test]
#[ignore = "Windows performance measurement; generate fixtures and run in Release"]
fn windows_performance_editor_long_paragraph() {
    measure_editor("editor_long_paragraph", "long-paragraph.md");
}

fn measure_editor(case: &str, fixture: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/editor-perf/fixtures")
        .join(fixture);
    let (file, source) = DocumentFile::open(&path, MAX_DOCUMENT_CHARACTERS).unwrap_or_else(|error| {
        panic!(
            "cannot open performance fixture {}: {error:?}; run Test/Generate-EditorPerfFixtures.ps1",
            path.display()
        )
    });
    let bytes = source.len();
    let mut markers = source.match_indices(EDIT_MARKER);
    let edit_at = markers.next().expect("fixture edit marker").0 + EDIT_MARKER.len();
    assert!(markers.next().is_none(), "fixture edit marker is unique");

    let (h, document) = Harness::new(|weak| OpenDocument::new(file, source, weak));
    let id = PaneId::FIRST;
    let state = h.live.states.of(id);
    let cache = &h.live.cache;
    h.window.set_tree_open(false);
    id.update_screen(&h.window, |screen| {
        screen.shown_width = 950.0;
        screen.shown_height = 620.0;
        screen.preview = true;
    });
    h.window.show().unwrap();
    state.borrow_mut().caret_source_byte = Some(edit_at);
    refresh_pane_from_state(
        &h.window,
        cache,
        &document,
        id,
        &state,
        &document.text.borrow(),
    );
    wait_for_layout(&h, case, id, "initial layout");

    eprintln!("perf_fixture case={case} bytes={bytes} edit_byte={edit_at} mode=horizontal_preview");
    let mut input = Vec::with_capacity(EDITS);
    let mut visible = Vec::with_capacity(EDITS);
    let mut complete = Vec::with_capacity(EDITS);
    for event in 0..=EDITS {
        let started = Instant::now();
        let inserted =
            insert_pane_text(&h.window, id, &document, &h.live.states, cache, "x", false);
        let returned = started.elapsed();
        assert!(inserted, "{case} accepted editor input");

        // A costly previous redraw can defer this edit to the catch-up timer.
        // The visible milestone is publication of the editor's DirectWrite tiles.
        while cache.borrow_mut().pace_of(id).waiting {
            assert!(
                started.elapsed() < WAIT_LIMIT,
                "{case} did not draw the edit"
            );
            std::thread::sleep(Duration::from_millis(1));
            slint::platform::update_timers_and_animations();
        }
        let drawn = started.elapsed();
        wait_for_layout(&h, case, id, "edit layout");
        let settled = started.elapsed();

        assert_eq!(state.borrow().caret_source_byte, Some(edit_at + event + 1));
        let text = document.text.borrow();
        assert_eq!(text.len(), bytes + event + 1);
        assert_eq!(text.as_bytes()[edit_at + event], b'x');
        assert!(
            id.screen(&h.window).tiles.row_count() > 0,
            "{case} drew tiles"
        );
        let status = h.window.get_render_status();
        assert!(!status.contains("NG"), "{case}: {status}");

        // The first edit warms the normal input and redraw paths.
        if event != 0 {
            input.push(returned);
            visible.push(drawn);
            complete.push(settled);
            eprintln!(
                "perf_sample case={case} event={event} input_return_ms={:.3} visible_layout_ms={:.3} layout_complete_ms={:.3}",
                ms(returned),
                ms(drawn),
                ms(settled)
            );
        }
    }
    report(case, "input_return", &mut input);
    report(case, "visible_layout", &mut visible);
    report(case, "layout_complete", &mut complete);
}

fn wait_for_layout(h: &Harness, case: &str, id: PaneId, stage: &str) {
    let started = Instant::now();
    let cache = &h.live.cache;
    while cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
        assert!(started.elapsed() < WAIT_LIMIT, "{case} stalled at {stage}");
        // Use the same collector as the application without waiting for its
        // periodic UI tick to notice a completed background layout.
        collect_layout_results(&h.window, &h.live.states, cache);
        if cache.borrow_mut().pane(id).graphics.engine.layout_pending() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
