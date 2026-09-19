//! Main-window embedding adapter. Registration and cleanup stay together so callers
//! cannot forget a state, model row, render cache or compatibility TAB entry.
use super::*;
use crate::editor_session::EditorSession;

pub(crate) struct EditorHost<'a> {
    window: &'a AppWindow,
    live: &'a Live,
}

impl<'a> EditorHost<'a> {
    pub(crate) fn new(window: &'a AppWindow, live: &'a Live) -> Self {
        Self { window, live }
    }

    pub(crate) fn attach(&self, owner: PaneId, session: EditorSession) -> Option<PaneId> {
        let model = self.window.get_panes();
        let rows = model.as_any().downcast_ref::<VecModel<PaneScreen>>()?;
        let next = self
            .live
            .states
            .next_panel
            .get()
            .max(PaneId::EMBEDDED_START);
        if next >= i32::MAX as u32 {
            return None;
        }
        self.live.states.next_panel.set(next + 1);
        let id = PaneId(next);
        let tab = PaneTab::showing(self.window, owner, session.document());
        self.live.states.panels.borrow_mut().insert(next, session);
        // Transitional adapter for existing main-window callbacks. The embedding
        // caller does not manufacture a TAB; standalone hosts must not need this.
        self.live.tabs.borrow_mut().panels.insert(
            next,
            PaneTabs {
                tabs: vec![tab],
                ..Default::default()
            },
        );
        let mut screen = id.initial_screen(false, false);
        screen.embedded_panel = true;
        screen.panel_owner = owner.index();
        rows.push(screen);
        Some(id)
    }

    pub(crate) fn hide_except(&self, visible: &[PaneId]) {
        let model = self.window.get_panes();
        let Some(rows) = model.as_any().downcast_ref::<VecModel<PaneScreen>>() else {
            return;
        };
        for row in 0..rows.row_count() {
            let mut screen = rows.row_data(row).unwrap();
            if screen.embedded_panel
                && !visible.contains(&PaneId::from_index(screen.id))
                && screen.width != 0.
            {
                screen.width = 0.;
                rows.set_row_data(row, screen);
            }
        }
    }

    /// Call after the host has transferred focus away from a closing surface.
    pub(crate) fn retain(&self, retained: &[PaneId]) {
        let model = self.window.get_panes();
        let Some(rows) = model.as_any().downcast_ref::<VecModel<PaneScreen>>() else {
            return;
        };
        for row in (0..rows.row_count()).rev() {
            let screen = rows.row_data(row).unwrap();
            if screen.embedded_panel && !retained.contains(&PaneId::from_index(screen.id)) {
                rows.remove(row);
            }
        }
        let keep = |id: &u32| retained.contains(&PaneId(*id));
        self.live
            .states
            .panels
            .borrow_mut()
            .retain(|id, _| keep(id));
        self.live.tabs.borrow_mut().panels.retain(|id, _| keep(id));
        let mut cache = self.live.cache.borrow_mut();
        cache.panel_panes.retain(|id, _| keep(id));
        cache.panel_pace.retain(|id, _| keep(id));
    }
}
