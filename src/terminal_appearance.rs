//! Kind defaults and tab overrides. Pane changes only reset paper overrides.
use super::*;
use crate::appearance::readable_ink;

pub(crate) fn initial_style() -> PanelStyle {
    PanelStyle {
        paper_own: true,
        paper: Color::from_rgb_u8(255, 254, 250),
        ink: Color::from_rgb_u8(36, 33, 30),
        family: "Consolas".into(),
        size: 15,
        ..Default::default()
    }
}

pub(crate) fn default_style(window: &AppWindow, kind: usize) -> PanelStyle {
    window
        .get_panel_defaults()
        .row_data(kind)
        .unwrap_or_else(|| PanelStyle {
            paper_own: true,
            paper: window.get_terminal_paper(),
            ink: window.get_terminal_ink(),
            family: window.get_terminal_font(),
            size: if kind == 1 {
                14
            } else {
                window.get_terminal_size()
            },
            ..Default::default()
        })
}

pub(crate) fn random_style(window: &AppWindow, kind: usize) -> Option<PanelStyle> {
    let mut style = default_style(window, kind);
    if style.random == 0 {
        return None;
    }
    style.paper = slint_colour(random_paper(style.random == 1, random_seed()));
    style.ink = slint_colour(readable_ink(channels(style.ink), channels(style.paper)));
    style.paper_own = true;
    Some(style)
}

pub(crate) fn publish(window: &AppWindow, id: PaneId, below: &TabBelow) {
    let screen = id.screen(window);
    let mut front = below
        .front_style
        .clone()
        .unwrap_or_else(|| default_style(window, 0));
    let base = default_style(window, 0);
    front.paper = if below.front_style.as_ref().is_some_and(|s| s.paper_own) {
        front.paper
    } else if screen.tab_paper_own {
        screen.tab_paper
    } else if screen.pane_paper_own {
        screen.pane_paper
    } else {
        base.paper
    };
    let entry = below.entries.get(below.active).map(|p| p.borrow());
    let kind = if screen.terminal { 1 } else { 2 };
    let base = default_style(window, kind);
    let mut panel = entry
        .as_ref()
        .and_then(|p| p.style.clone())
        .unwrap_or_else(|| base.clone());
    panel.paper = if entry
        .as_ref()
        .is_some_and(|p| p.style.as_ref().is_some_and(|s| s.paper_own))
    {
        panel.paper
    } else if screen.pane_paper_own {
        screen.pane_paper
    } else {
        base.paper
    };
    if screen.front_style == front && screen.panel_style == panel {
        return;
    }
    id.update_screen(window, |screen| {
        screen.front_style = front.clone();
        screen.panel_style = panel.clone();
    });
}

pub(crate) fn look(window: &AppWindow, id: PaneId, spot: TerminalSpot) -> cells::TerminalLook {
    let screen = id.screen(window);
    let style = if spot == TerminalSpot::Front {
        screen.front_style
    } else {
        screen.panel_style
    };
    if style.size == 0 {
        return terminal_look(window);
    }
    let paper = channels(style.paper);
    cells::TerminalLook {
        family: style.family.to_string(),
        font_size: style.size.clamp(8, 72) as f32,
        paper,
        ink: channels(style.ink),
        palette: cells::TerminalLook::palette_for(paper),
        transparent: style.transparency > 0,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        ..Default::default()
    }
}

pub(crate) fn clear_paper(tab: &mut PaneTab) {
    if let Some(style) = &mut tab.below.front_style {
        style.paper_own = false;
    }
    for entry in &tab.below.entries {
        if let Some(style) = &mut entry.borrow_mut().style {
            style.paper_own = false;
        }
    }
}

#[derive(Clone)]
enum Target {
    Default(usize),
    Front(Rc<()>),
    Panel(Rc<RefCell<terminal_panels::PanelDocument>>),
}

pub(crate) fn install(window: &AppWindow, live: &Live) {
    let weak = window.as_weak();
    window.on_font_list_requested(move || {
        if let Some(window) = weak.upgrade() {
            wiring::fill_font_names(&window);
        }
    });
    let weak = window.as_weak();
    let settings_live = live.clone();
    let timer = Timer::default();
    window.on_terminal_settings_edited(move || {
        let weak = weak.clone();
        let live = settings_live.clone();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(200),
            move || {
                let Some(window) = weak.upgrade() else {
                    return;
                };
                for id in PaneId::all(&window) {
                    if let Some(tab) = live.tabs.borrow().of(id).current() {
                        publish(&window, id, &tab.below);
                    }
                }
                refresh_terminal_panes(&window, &live);
                save_settings(&window, &live.cache);
            },
        );
    });
    let target = Rc::new(RefCell::new(None::<Target>));
    let weak = window.as_weak();
    let opened_live = live.clone();
    let held = target.clone();
    window.on_appearance_requested(move |at, scope| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let (target, style, title) = if scope == 0 {
            let kind = at.clamp(0, 2) as usize;
            (
                Target::Default(kind),
                default_style(&window, kind),
                ["Terminal", "Editor Panel", "Terminal Panel"][kind],
            )
        } else {
            let id = PaneId::from_index(at);
            if scope == 2 {
                terminal_panels::ensure(&window, &opened_live, id);
                let Some(entry) = terminal_panels::current(&opened_live, id) else {
                    return;
                };
                (
                    Target::Panel(entry),
                    id.screen(&window).panel_style,
                    "Panel TAB",
                )
            } else {
                let identity = opened_live
                    .tabs
                    .borrow()
                    .of(id)
                    .current()
                    .map(|t| t.identity.clone());
                let Some(identity) = identity else {
                    return;
                };
                (
                    Target::Front(identity),
                    id.screen(&window).front_style,
                    "Terminal TAB",
                )
            }
        };
        window.set_appearance_default(scope == 0);
        window.set_appearance_underline_allowed(
            !matches!(&target, Target::Default(1))
                && !(scope == 2 && PaneId::from_index(at).screen(&window).terminal),
        );
        window.set_appearance_title(format!("{title} — {}", pick("外観", "Appearance")).into());
        window.set_appearance_style(style);
        *held.borrow_mut() = Some(target);
        window.set_appearance_open(true);
    });
    let weak = window.as_weak();
    let reset_live = live.clone();
    let reset_target = target.clone();
    window.on_appearance_reset(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let Some(target) = reset_target.borrow_mut().take() else {
            return;
        };
        match target {
            Target::Default(kind) => {
                let mut defaults: Vec<_> = (0..3).map(|k| default_style(&window, k)).collect();
                defaults[kind] = PanelStyle {
                    paper_own: true,
                    paper: Color::from_rgb_u8(255, 254, 250),
                    ink: Color::from_rgb_u8(36, 33, 30),
                    family: "Consolas".into(),
                    size: if kind == 1 { 14 } else { 15 },
                    ..Default::default()
                };
                window.set_panel_defaults(ModelRc::new(VecModel::from(defaults)));
            }
            Target::Front(identity) => {
                for tab in reset_live
                    .tabs
                    .borrow_mut()
                    .panes
                    .iter_mut()
                    .flat_map(|p| &mut p.tabs)
                {
                    if Rc::ptr_eq(&tab.identity, &identity) {
                        tab.paper = [None; 2];
                        tab.below.front_style = None;
                    }
                }
            }
            Target::Panel(entry) => entry.borrow_mut().style = None,
        }
        window.set_appearance_open(false);
        publish_tabs(&window, &reset_live);
        refresh_terminal_panes(&window, &reset_live);
        save_settings(&window, &reset_live.cache);
    });
    let weak = window.as_weak();
    let live = live.clone();
    window.on_appearance_applied(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let Some(target) = target.borrow_mut().take() else {
            return;
        };
        let mut style = window.get_appearance_style();
        if !matches!(&target, Target::Default(_)) {
            style.random = 0;
        }
        if style.family.trim().is_empty() {
            return;
        }
        match target {
            Target::Default(kind) => {
                let mut defaults: Vec<_> = (0..3).map(|k| default_style(&window, k)).collect();
                defaults[kind] = style;
                window.set_panel_defaults(ModelRc::new(VecModel::from(defaults)));
            }
            Target::Front(identity) => {
                for tab in live
                    .tabs
                    .borrow_mut()
                    .panes
                    .iter_mut()
                    .flat_map(|p| &mut p.tabs)
                {
                    if Rc::ptr_eq(&tab.identity, &identity) {
                        tab.paper = [None; 2];
                        tab.below.front_style = Some(style.clone());
                    }
                }
            }
            Target::Panel(entry) => entry.borrow_mut().style = Some(style),
        }
        window.set_appearance_open(false);
        publish_tabs(&window, &live);
        for id in PaneId::all(&window) {
            if let Some(tab) = live.tabs.borrow().of(id).current() {
                terminal_panels::publish(&window, id, &tab.below);
            }
        }
        refresh_terminal_panes(&window, &live);
        save_settings(&window, &live.cache);
    });
}

pub(crate) fn encode(style: &PanelStyle) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        hex_colour(style.paper),
        hex_colour(style.ink),
        style.size,
        style.transparency,
        style.bold,
        style.italic,
        style.underline,
        style.random,
        style.family
    )
}
pub(crate) fn decode(text: &str) -> Option<PanelStyle> {
    let fields: Vec<_> = text.splitn(9, '\t').collect();
    if fields.len() != 9 {
        return None;
    }
    Some(PanelStyle {
        paper_own: true,
        paper: slint_colour(parse_hex_colour(fields[0])?),
        ink: slint_colour(parse_hex_colour(fields[1])?),
        size: fields[2].parse::<i32>().ok()?.clamp(8, 72),
        transparency: fields[3].parse::<i32>().ok()?.clamp(0, 100),
        bold: fields[4].parse().ok()?,
        italic: fields[5].parse().ok()?,
        underline: fields[6].parse().ok()?,
        random: fields[7].parse::<i32>().ok()?.clamp(0, 2),
        family: fields[8].into(),
    })
}
