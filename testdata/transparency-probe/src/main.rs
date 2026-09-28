#![windows_subsystem = "windows"]

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Timer, TimerMode};
use std::time::Duration;

slint::slint! {
    import { Button, Slider, ScrollView } from "std-widgets.slint";
    export component Backdrop inherits Window {
        title: "Transparency probe - moving background";
        no-frame: true;
        preferred-width: 1280px; preferred-height: 900px;
        background: #175e94;
        in property <int> tick;
        Rectangle { x: mod(root.tick * 9, 760) * 1px; y: 80px;
            width: 200px; height: parent.height - 80px; background: #e5a93f; }
        Text { x: 30px; y: 24px; text: "BEHIND / 背後の別ウィンドウ  " + root.tick;
            color: white; font-size: 24px; }
    }
    export component Probe inherits Window {
        title: "RFN transparency probe";
        no-frame: true;
        preferred-width: 760px; preferred-height: 520px;
        min-width: 650px; min-height: 400px;
        background: transparent;
        in-out property <float> paper-alpha: 0.65;
        in-out property <string> input-text: "日本語入力の確認：ここをクリックして入力";
        in property <int> frame;
        callback drag-window();
        callback resize-window();
        callback close-probe();
        callback benchmark();
        VerticalLayout {
            spacing: 0px;
            Rectangle {
                height: 48px; background: #263747;
                Text { x: 16px; text: "RFN / 背景だけ透過  —  この帯をドラッグ";
                    color: white; vertical-alignment: center; }
                TouchArea { pointer-event(event) => { if event.kind == PointerEventKind.down { root.drag-window(); } } }
            }
            Rectangle {
                height: 52px; background: #edf0f3;
                HorizontalLayout {
                    padding: 8px; spacing: 12px;
                    Text { text: "背景の不透明度 " + round(root.paper-alpha * 100) + "%"; color: #162635; }
                    Slider { minimum: 0.10; maximum: 1; value <=> root.paper-alpha; }
                    Button { text: "サイズ"; clicked => { root.resize-window(); } }
                    Button { text: "計測"; clicked => { root.benchmark(); } }
                    Button { text: "終了"; clicked => { root.close-probe(); } }
                }
            }
            Rectangle {
                background: #ffffff.with-alpha(root.paper-alpha);
                VerticalLayout {
                    padding: 18px; spacing: 14px;
                    Text { text: "文字とメニューは不透明。背景は別ウィンドウです。 " + root.frame;
                        color: #102030; font-size: 21px; wrap: word-wrap; }
                    Rectangle { height: 60px; border-width: 1px; border-color: #102030;
                        background: transparent;
                        TextInput { x: 8px; y: 8px; width: parent.width - 16px; height: parent.height - 16px;
                            text <=> root.input-text; color: #102030; font-size: 22px; single-line: true; }
                    }
                    ScrollView {
                        viewport-height: 1100px;
                        for row in 30 : Text {
                            x: 8px; y: row * 34px; text: "本文 " + row + "   半透明の紙に、不透明な文字を描きます。";
                            color: #102030; font-size: 20px;
                        }
                    }
                }
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(
        std::env::current_exe()?.with_extension("log"),
        "Transparency probe: new run\n",
    )?;
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    let back = Backdrop::new()?;
    back.window()
        .set_position(slint::PhysicalPosition::new(180, 120));
    back.show()?;
    let probe = Probe::new()?;
    probe
        .window()
        .set_position(slint::PhysicalPosition::new(250, 215));
    let weak = probe.as_weak();
    probe.on_drag_window(move || {
        if let Some(p) = weak.upgrade() {
            p.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
    });
    let weak = probe.as_weak();
    probe.on_resize_window(move || {
        if let Some(p) = weak.upgrade() {
            let wide = p.window().size().width < 900;
            p.window().set_size(slint::PhysicalSize::new(
                if wide { 1050 } else { 760 },
                if wide { 700 } else { 520 },
            ));
        }
    });
    probe.on_close_probe(|| slint::quit_event_loop().unwrap());
    let benchmark = std::rc::Rc::new(Timer::default());
    let weak = probe.as_weak();
    let benchmark_start = benchmark.clone();
    probe.on_benchmark(move || {
        let weak = weak.clone();
        let remaining = std::cell::Cell::new(180);
        let stop = std::rc::Rc::downgrade(&benchmark_start);
        benchmark_start.start(TimerMode::Repeated, Duration::from_millis(16), move || {
            if let Some(p) = weak.upgrade() {
                p.set_frame(p.get_frame() + 1);
            }
            remaining.set(remaining.get() - 1);
            if remaining.get() == 0 {
                if let Some(timer) = stop.upgrade() {
                    timer.stop();
                }
            }
        });
    });
    probe.show()?;
    let timer = Timer::default();
    let weak_back = back.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        if let Some(b) = weak_back.upgrade() {
            b.set_tick(b.get_tick() + 1);
        }
    });
    slint::run_event_loop()?;
    Ok(())
}
