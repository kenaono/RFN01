//! Comparison and explicitly staged merge, preserving the editor's tabs and views.
use crate::{AppWindow, DiffRow, MAX_DOCUMENT_CHARACTERS, buffer::DocumentFile};
use slint::Model;
use slint::{ComponentHandle, ModelRc, VecModel};
use std::path::Path;
use std::{cell::RefCell, rc::Rc};

struct Aligned {
    rows: Vec<DiffRow>,
    starts: Vec<usize>,
    grouped: bool,
}

fn aligned(left: &str, right: &str) -> Aligned {
    let difference = crate::comparison::compare(left, right);
    let mut result = Aligned {
        rows: Vec::new(),
        starts: Vec::new(),
        grouped: difference.grouped,
    };
    let (mut left_number, mut right_number) = (0, 0);
    let mut append = |a: &str, b: &str, changed: bool, result: &mut Aligned| {
        let a: Vec<_> = a.split_inclusive('\n').collect();
        let b: Vec<_> = b.split_inclusive('\n').collect();
        if changed {
            result.starts.push(result.rows.len());
        }
        for index in 0..a.len().max(b.len()) {
            let l = a.get(index).copied();
            let r = b.get(index).copied();
            let number = |present, count: &mut usize| {
                if present {
                    *count += 1;
                    count.to_string().into()
                } else {
                    slint::SharedString::default()
                }
            };
            result.rows.push(DiffRow {
                decision: 0,
                hunk_start: changed && index == 0,
                left: l.map(display_line).unwrap_or_default().into(),
                right: r.map(display_line).unwrap_or_default().into(),
                left_number: number(l.is_some(), &mut left_number),
                right_number: number(r.is_some(), &mut right_number),
                kind: if !changed {
                    0
                } else if r.is_none() {
                    1
                } else if l.is_none() {
                    2
                } else {
                    3
                },
            });
        }
    };
    let (mut a, mut b) = (0, 0);
    for (l, r) in difference.pairs {
        append(&left[a..l.start], &right[b..r.start], false, &mut result);
        append(&left[l.clone()], &right[r.clone()], true, &mut result);
        a = l.end;
        b = r.end;
    }
    append(&left[a..], &right[b..], false, &mut result);
    result
}

fn display_line(line: &str) -> String {
    let (body, end) = if let Some(body) = line.strip_suffix("\r\n") {
        (body, " ［CRLF］")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, " ［改行］")
    } else {
        (line, " ［改行なし］")
    };
    format!("{}{end}", body.replace('\t', "→   "))
}

fn step(starts: &[usize], current: i32, forward: bool) -> Option<usize> {
    if forward {
        starts.iter().copied().find(|at| *at as i32 > current)
    } else {
        starts
            .iter()
            .rev()
            .copied()
            .find(|at| (*at as i32) < current)
    }
}

fn populate(window: &AppWindow, left: String, right: String) {
    let aligned = aligned(&left, &right);
    let count = aligned.starts.len();
    let summary = if count == 0 {
        "差分はありません".to_owned()
    } else {
        format!(
            "差分 {count} 箇所{}",
            if aligned.grouped {
                "・広い変更をまとめて表示"
            } else {
                ""
            }
        )
    };
    let width = aligned
        .rows
        .iter()
        .map(|row| row.left.chars().count().max(row.right.chars().count()))
        .max()
        .unwrap_or(0);
    window.set_diff_content_width(((width * 14 + 72) * 2) as f32);
    let starts = aligned.starts.clone();
    let kinds: Vec<_> = aligned.rows.iter().map(|row| row.kind).collect();
    let click_summary = summary.clone();
    let weak = window.as_weak();
    window.on_diff_select_difference(move |row| {
        let Ok(row) = usize::try_from(row) else {
            return;
        };
        if kinds.get(row).is_none_or(|kind| *kind == 0) {
            return;
        }
        let Some(index) = starts.iter().rposition(|start| *start <= row) else {
            return;
        };
        if let Some(window) = weak.upgrade() {
            window.set_diff_selected_row(starts[index] as i32);
            window.set_diff_can_previous(index > 0);
            window.set_diff_can_next(index + 1 < starts.len());
            window.set_diff_status(format!("{click_summary}（{}/{count}）", index + 1).into());
        }
    });
    window.set_diff_rows(ModelRc::new(VecModel::from(aligned.rows)));
    window.set_diff_selected_row(-1);
    window.set_diff_can_previous(false);
    window.set_diff_can_next(count > 0);
    window.set_diff_status(summary.clone().into());
    let weak = window.as_weak();
    window.on_diff_next_difference(move |forward| {
        if let Some(window) = weak.upgrade() {
            if let Some(row) = step(&aligned.starts, window.get_diff_selected_row(), forward) {
                window.set_diff_selected_row(row as i32);
                let index = aligned.starts.iter().position(|at| *at == row).unwrap() + 1;
                window.set_diff_can_previous(index > 1);
                window.set_diff_can_next(index < count);
                window.set_diff_status(format!("{summary}（{index}/{count}）").into());
            } else {
                window.set_diff_status(
                    if count == 0 {
                        "差分はありません"
                    } else if forward {
                        "これより先の差分はありません"
                    } else {
                        "これより前の差分はありません"
                    }
                    .into(),
                );
            }
        }
    });
    let weak = window.as_weak();
    window.on_diff_copy_side(move |right_side| {
        if let Some(window) = weak.upgrade() {
            let copied = crate::clipboard::put_text(None, if right_side { &right } else { &left });
            window.set_diff_status(
                if copied {
                    "本文をコピーしました"
                } else {
                    "コピーできませんでした"
                }
                .into(),
            );
        }
    });
}

fn read(path: &Path) -> Result<String, String> {
    DocumentFile::open(path, MAX_DOCUMENT_CHARACTERS)
        .map(|(_, text)| text)
        .map_err(|error| format!("{}を開けません: {error}", path.display()))
}

fn dismiss(app: &AppWindow) {
    app.set_diff_merge_enabled(false);
    app.set_diff_can_apply(false);
    app.on_diff_choose_side(|_| {});
    app.on_diff_apply_merge(|| {});
    app.set_diff_active(false);
    app.set_diff_rows(Default::default());
    app.on_diff_copy_side(|_| {});
    app.on_diff_next_difference(|_| {});
    app.on_diff_select_difference(|_| {});
    crate::restore_editor_focus(app);
}

pub fn show(app: &AppWindow, left_label: String, left: String, right_label: String, right: String) {
    app.on_diff_choose_side(|_| {});
    app.on_diff_apply_merge(|| {});
    app.set_diff_merge_enabled(false);
    app.set_diff_can_apply(false);
    app.set_diff_left_path(left_label.into());
    app.set_diff_right_path(right_label.into());
    populate(app, left, right);
    app.set_diff_active(true);
}

fn merged_text(left: &str, right: &str, choices: &[bool]) -> String {
    let pairs = crate::comparison::compare(left, right).pairs;
    let mut merged = left.to_owned();
    for ((a, b), chosen) in pairs.into_iter().zip(choices).rev() {
        if *chosen {
            merged.replace_range(a, &right[b]);
        }
    }
    merged
}

#[allow(clippy::too_many_arguments)]
pub fn show_merge(
    app: &AppWindow,
    live: &crate::Live,
    pane: crate::PaneId,
    document: Rc<crate::OpenDocument>,
    left_label: String,
    left: String,
    right_label: String,
    right: String,
) {
    show(app, left_label, left.clone(), right_label, right.clone());
    if document.read_only()
        || live.states.of(pane).borrow().viewer
        || !Rc::ptr_eq(&live.states.document(pane), &document)
    {
        return;
    }
    app.set_diff_merge_enabled(true);
    let view = aligned(&left, &right);
    let choices = Rc::new(RefCell::new(vec![false; view.starts.len()]));
    let rows = Rc::new(VecModel::from(view.rows));
    app.set_diff_rows(ModelRc::from(rows.clone()));
    let weak = app.as_weak();
    let selected = choices.clone();
    app.on_diff_choose_side(move |right| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let Some(index) = view
            .starts
            .iter()
            .position(|row| *row as i32 == app.get_diff_selected_row())
        else {
            return;
        };
        selected.borrow_mut()[index] = right;
        let start = view.starts[index];
        let end = view
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(rows.row_count());
        for at in start..end {
            let Some(mut row) = rows.row_data(at) else {
                break;
            };
            if row.kind == 0 {
                break;
            }
            row.decision = if right { 2 } else { 1 };
            rows.set_row_data(at, row);
        }
        let count = selected.borrow().iter().filter(|chosen| **chosen).count();
        app.set_diff_can_apply(count > 0);
        app.set_diff_status(
            format!("右を採用：{count} 箇所（まだ本文（左）には反映していません）").into(),
        );
    });
    let weak = app.as_weak();
    let live = live.clone();
    app.on_diff_apply_merge(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        if !Rc::ptr_eq(&live.states.document(pane), &document) || *document.text.borrow() != left {
            app.set_diff_status("比較開始後に本文が変わりました。比較を開き直してください".into());
            return;
        }
        if document.read_only() || live.states.of(pane).borrow().viewer {
            app.set_diff_status("Viewerでは取り込めません".into());
            return;
        }
        let merged = merged_text(&left, &right, &choices.borrow());
        if merged == left {
            return;
        }
        if merged.chars().count() > MAX_DOCUMENT_CHARACTERS {
            app.set_diff_status("文字数の上限を超えるため反映できません".into());
            return;
        }
        let caret = live
            .states
            .of(pane)
            .borrow()
            .caret_source_byte
            .unwrap_or(0)
            .min(merged.len());
        document.history.borrow_mut().separate_next = true;
        crate::apply_span_edit(
            &app,
            &live,
            pane,
            &left,
            0..left.len(),
            &merged,
            (caret, caret),
            "選択した差分を本文（左）に反映しました。Undoで戻せます",
        );
        document.history.borrow_mut().separate_next = true;
        app.set_diff_active(false);
        crate::restore_editor_focus(&app);
        // A Slint callback cannot replace itself while it is executing.
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(app) = weak.upgrade()
                && !app.get_diff_active()
            {
                dismiss(&app);
            }
        });
    });
}

fn show_saved(app: &AppWindow, document: &crate::OpenDocument) {
    // Reading a clone preserves the document's agreed stamp and encoding settings.
    let mut file = document.file.borrow().clone();
    let Some(path) = file.path().map(Path::to_owned) else {
        app.set_render_status("まだ保存先がありません。保存してから比較してください".into());
        return;
    };
    match file.reload(MAX_DOCUMENT_CHARACTERS) {
        Some(Ok(right)) => show(
            app,
            format!("本文（左）：{}", path.display()),
            document.text.borrow().clone(),
            format!("保存版（取得時点）：{}", path.display()),
            right,
        ),
        Some(Err(error)) => {
            app.set_render_status(format!("保存版を比較できません: {error}").into())
        }
        None => {}
    }
}

pub fn wire(app: &AppWindow, live: &crate::Live) {
    let weak = app.as_weak();
    let saved_live = live.clone();
    app.on_compare_saved_requested(move || {
        if let Some(app) = weak.upgrade() {
            show_saved(&app, &saved_live.active(&app));
        }
    });
    let weak = app.as_weak();
    let live = live.clone();
    app.on_compare_files_requested(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let document = live.active(&app);
        let pane = crate::focused_pane(&app);
        let left = document.text.borrow().clone();
        let label = document
            .file
            .borrow()
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "未保存の文書".into());
        let weak = weak.clone();
        let live = live.clone();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            let owner = crate::ime::window_handle(&app);
            let Some(path) =
                crate::file_dialog::open_document_named(owner, "比較対象のファイルを選択")
            else {
                return;
            };
            match read(&path) {
                Ok(right) => {
                    show_merge(
                        &app,
                        &live,
                        pane,
                        document,
                        format!("{label}（編集中の本文）"),
                        left,
                        format!("{}（保存内容）", path.display()),
                        right,
                    );
                }
                Err(error) => app.set_render_status(error.into()),
            }
        });
    });
    let weak = app.as_weak();
    app.on_diff_dismissed(move || {
        if let Some(app) = weak.upgrade() {
            dismiss(&app);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_only_selected_hunks_and_preserve_original_newlines() {
        let left = "猫\n一\n削除\n二\n終\n";
        let right = "犬\n一\n二\n追加\n終\n";
        assert_eq!(crate::comparison::compare(left, right).pairs.len(), 3);
        assert_eq!(
            merged_text(left, right, &[true, false, true]),
            "犬\n一\n削除\n二\n追加\n終\n"
        );
        assert_eq!(merged_text(left, right, &[true, true, true]), right);
        assert_eq!(merged_text(left, right, &[false, false, false]), left);
        assert_eq!(merged_text("猫😀\r\n", "犬🐕", &[true]), "犬🐕");
        assert_eq!(merged_text("", "追加\r\n", &[true]), "追加\r\n");
        assert_eq!(merged_text("削除\r\n", "", &[true]), "");
    }
    #[test]
    #[ignore = "offscreen comparison window and click verification"]
    fn diff_window_renders_and_navigates() {
        use slint::platform::{
            PointerEventButton, WindowEvent, software_renderer::MinimalSoftwareWindow,
        };
        struct Offscreen(Rc<MinimalSoftwareWindow>);
        impl slint::platform::Platform for Offscreen {
            fn create_window_adapter(
                &self,
            ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }
        let surface = MinimalSoftwareWindow::new(Default::default());
        slint::platform::set_platform(Box::new(Offscreen(surface.clone()))).unwrap();
        let window = AppWindow::new().unwrap();
        let weak = window.as_weak();
        window.on_diff_dismissed(move || dismiss(&weak.unwrap()));
        surface.set_size(slint::PhysicalSize::new(1120, 740));
        window.set_diff_left_path("28_diff_左.md".into());
        window.set_diff_right_path("28_diff_右.md".into());
        populate(
            &window,
            "# 朝の庭\n猫が眠っている。\n風が木々を揺らす。\n削除される行。\n夜になった。\n末尾"
                .into(),
            "# 朝の庭\n犬が眠っている。\n風が木々を揺らす。\n夜になった。\n追加された行。\n末尾\n"
                .into(),
        );
        window.set_diff_active(true);
        window.show().unwrap();
        let click = |x, y| {
            let position = slint::LogicalPosition::new(x, y);
            for event in [
                WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                },
                WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                },
            ] {
                window.window().dispatch_event(event);
            }
        };
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1120 * 740];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1120);
        });
        click(800.0, 195.0);
        assert_eq!(
            window.get_diff_selected_row(),
            3,
            "right-side click selects deletion"
        );
        click(100.0, 143.0);
        assert_eq!(
            window.get_diff_selected_row(),
            1,
            "left-side click selects replacement"
        );
        click(100.0, 117.0);
        assert_eq!(
            window.get_diff_selected_row(),
            1,
            "unchanged line keeps selection"
        );
        window.set_diff_selected_row(-1);
        assert!(!window.get_diff_can_previous());
        click(50.0, 22.0);
        assert_eq!(window.get_diff_selected_row(), -1);
        click(145.0, 22.0);
        let first = window.get_diff_selected_row();
        assert!(first >= 0, "next button reaches the comparison");
        click(145.0, 22.0);
        assert!(window.get_diff_selected_row() > first);
        click(50.0, 22.0);
        assert_eq!(window.get_diff_selected_row(), first);
        assert!(!window.get_diff_can_previous());
        window.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(145.0, 22.0),
        });
        window.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1120);
        });
        let output = std::path::Path::new("target/diff-qa");
        std::fs::create_dir_all(output).unwrap();
        let mut ppm = b"P6\n1120 740\n255\n".to_vec();
        for pixel in pixels {
            ppm.extend([pixel.r, pixel.g, pixel.b]);
        }
        std::fs::write(output.join("comparison.ppm"), ppm).unwrap();
        while window.get_diff_can_next() {
            click(145.0, 22.0);
        }
        let last = window.get_diff_selected_row();
        click(145.0, 22.0);
        assert_eq!(window.get_diff_selected_row(), last);
        assert!(window.get_diff_can_previous());
        populate(&window, "同じ".into(), "同じ".into());
        assert!(!window.get_diff_can_previous());
        assert!(!window.get_diff_can_next());
        click(235.0, 22.0);
        assert!(!window.get_diff_active(), "exit button restores the editor");
        window.set_diff_active(true);
        window.window().request_redraw();
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut vec![slint::Rgb8Pixel::default(); 1120 * 740], 1120);
        });
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::Escape.into(),
        });
        assert!(!window.get_diff_active(), "Escape exits the comparison");
    }
    #[test]
    fn aligns_insertions_and_preserves_line_numbers() {
        let view = aligned("朝\n夜\n", "朝\n昼\n夜\n");
        assert_eq!(view.starts, vec![1]);
        assert_eq!(view.rows.len(), 3);
        assert_eq!(view.rows[1].kind, 2);
        assert!(view.rows[1].left_number.is_empty());
        assert_eq!(view.rows[2].left_number, "2");
        assert_eq!(view.rows[2].right_number, "3");
        assert_eq!(step(&view.starts, -1, true), Some(1));
        assert_eq!(step(&view.starts, 1, true), None);
    }
    #[test]
    fn deletion_replacement_and_final_newline_remain_visible() {
        let view = aligned("猫\n削除\n終", "犬\n終\n");
        assert!(view.rows.iter().any(|row| row.kind == 1));
        assert!(view.rows.iter().any(|row| row.kind == 3));
        assert_ne!(display_line("終"), display_line("終\n"));
        assert!(aligned("", "").rows.is_empty());
        assert!(aligned("同じ\n", "同じ\n").starts.is_empty());
        let swapped = aligned("朝\n昼\n夜\n", "朝\n夜\n");
        assert_eq!(swapped.rows[1].kind, 1);
    }
}
