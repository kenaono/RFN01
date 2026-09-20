//! Embedded Editor Panel views use the same EditorPane, TextEngine and input callbacks.
//! The tagged ids keep their state independent of the structural pane layout.
use super::*;

pub(crate) fn sync(window: &AppWindow, live: &Live) {
    let showing: Vec<_> = PaneId::all(window).into_iter().filter_map(|owner| {
        let tabs = live.tabs.borrow();
        let tab = tabs.of(owner).current()?;
        if !owner.screen(window).terminal { return None; }
        let entry = tab.below.entries.get(tab.below.active)?.clone();
        Some((owner, entry, owner.screen(window).below_kind == 2))
    }).collect();
    let host = editor_host::EditorHost::new(window, live);
    let mut visible = Vec::new();
    for (owner, entry, open) in showing {
        let mut panel = entry.borrow_mut();
        let id = if let Some(id) = panel.source_id { id } else {
            let session = editor_session::EditorSession::with_state(panel.document.clone(), panel.view.clone());
            let Some(id) = host.attach(owner, session) else { continue; };
            panel.source_id = Some(id);
            id
        };
        let state = live.states.of(id);
        visible.push(id);
        let source = panel.document.text.borrow().clone();
        let document = panel.document.clone();
        drop(panel);
        let parent = owner.screen(window);
        id.update_screen(window, |s| {
            s.panel_owner = owner.index();
            s.panel_style = parent.panel_style.clone();
            s.viewer = state.borrow().viewer;
            s.paper_h_own = true;
            s.paper_h = parent.panel_style.paper;
            s.width = if open && parent.below_kind == 2 { parent.width } else { 0. };
            s.height = parent.below_height;
            s.x = parent.x;
            s.y = parent.y + parent.height - parent.below_height;
        });
        if id.screen(window).width > 0. {
            let follow = state.borrow().viewer && state.borrow().follow;
            refresh_pane_from_state(window, &live.cache, &document, id, &state, &source);
            if follow {
                scroll_to_end(window, &live.cache, id);
                refresh_after_scroll(window, &live.cache, id, id.scroll(window));
            }
        }
    }
    host.hide_except(&visible);
    let retained: Vec<_> = terminal_panels::entries(live).iter()
        .filter_map(|entry| entry.borrow().source_id).collect();
    let focused = focused_pane(window);
    if focused.is_panel() && (!visible.contains(&focused) || focused.screen(window).width <= 0.) {
        let owner = PaneId::from_index(focused.screen(window).panel_owner);
        focus(window, live, owner, owner.screen(window).below_kind == 2);
    }
    host.retain(&retained);
}

pub(crate) fn close(window: &AppWindow, live: &Live, id: PaneId) -> bool {
    let owner = PaneId::from_index(id.screen(window).panel_owner);
    terminal_panels::action(window, live, owner, 2, 0);
    true
}

pub(crate) fn focus(window: &AppWindow, live: &Live, owner: PaneId, into: bool) {
    let target = if into {
        terminal_panels::current(live, owner).and_then(|entry| entry.borrow().source_id).unwrap_or(owner)
    } else { owner };
    window.set_focused_pane_row(target.row(window) as i32);
    window.set_focused_pane(target.index());
    restore_editor_focus(window);
}
