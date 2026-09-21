//! Title-row client content with native Windows move/resize/caption commands.
//! Never call Slint while servicing a native frame callback: resizing can reenter.
use std::cell::Cell;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Dwm::DwmDefWindowProc,
    Graphics::Gdi::ScreenToClient,
    UI::{
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        Input::KeyboardAndMouse::{
            ReleaseCapture, SetCapture, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const SUBCLASS_ID: usize = 0x52464e41;
pub const TITLE_HEIGHT: f32 = 36.;
pub const BUTTON_WIDTH: f32 = 46.;

/// Private message carrying the caption-button state to Slint. The frame cannot
/// call Slint itself (resizing reenters), so the state travels the same way the
/// system commands do and the UI is set from the message queue instead.
const WM_CAPTION_STATE: u32 = WM_APP + 1;

pub fn window_handle(window: &super::AppWindow) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::{ComponentHandle, winit_030::WinitWindowAccessor};
    window
        .window()
        .with_winit_window(|native| match native.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut _)),
            _ => None,
        })
        .flatten()
}

pub struct Chrome {
    hwnd: HWND,
    window: slint::Weak<super::AppWindow>,
    // UI geometry, logical pixels. Only accessed on the window's owning thread.
    interactive_end: Cell<f32>,
    // 1 minimise, 2 maximise, 3 close, 0 nothing: which caption button the
    // pointer is over and which one is held down.
    hot: Cell<u32>,
    pressed: Cell<u32>,
    // The pair last posted, so a mouse move that changes nothing stays silent.
    sent: Cell<(u32, u32)>,
}

impl Chrome {
    pub fn install(
        hwnd: HWND,
        window: slint::Weak<super::AppWindow>,
    ) -> Result<Box<Self>, windows::core::Error> {
        let chrome = Box::new(Self {
            hwnd,
            window,
            interactive_end: Cell::new(36.),
            hot: Cell::new(0),
            pressed: Cell::new(0),
            sent: Cell::new((0, 0)),
        });
        unsafe {
            SetWindowSubclass(
                hwnd,
                Some(frame_proc),
                SUBCLASS_ID,
                (&*chrome as *const Self) as usize,
            )
            .ok()?;
            if let Err(error) = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            ) {
                let _ = RemoveWindowSubclass(hwnd, Some(frame_proc), SUBCLASS_ID);
                return Err(error);
            }
        }
        Ok(chrome)
    }

    pub fn set_interactive_end(&self, x: f32) {
        self.interactive_end.set(x.max(36.));
    }

    /// Send the caption state to the UI if it differs from the last one posted.
    fn publish(&self) {
        let state = (self.hot.get(), self.pressed.get());
        if self.sent.replace(state) == state {
            return;
        }
        unsafe {
            let _ = PostMessageW(
                Some(self.hwnd),
                WM_CAPTION_STATE,
                WPARAM(state.0 as usize),
                LPARAM(state.1 as isize),
            );
        }
    }

    /// Ask Windows for WM_NCMOUSELEAVE, so a pointer that leaves the window
    /// straight off a caption button still clears the highlight.
    fn track_leave(&self) {
        let mut track = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE | TME_NONCLIENT,
            hwndTrack: self.hwnd,
            dwHoverTime: u32::MAX,
        };
        unsafe {
            let _ = TrackMouseEvent(&mut track);
        }
    }
}

/// Put the remembered place back on the window itself (要件 8.5, RFN01-40).
///
/// [`super::session::restore_window_place`] asks Slint for the size before the
/// window exists, and winit works the outer size out from the window style — a
/// style that still carries the caption this chrome paints over. The window
/// therefore came back a caption taller on every run, and the height grew by
/// that much again on the next one.
///
/// Here the difference the window actually has is measured, and the outer
/// rectangle is set with it. The place keeps the outer position and the client
/// size, so the frame goes back on and the client the writer left is what comes
/// back — on whatever monitor's DPI the window landed on.
/// この状態の窓に、覚えていた位置と大きさを入れてよいか（RFN01-45）。
///
/// **最大化中・最小化中は入れない。**ここで入れるのは「元の大きさ」であり、
/// Windowsはそれを自分で持っている。状態と矩形は別のものなので、最大化された窓へ
/// 入れ直すと「状態は最大化、大きさは通常」の食い違いになり、元の大きさへ戻すと
/// さらに小さくなる。
fn wants_place(zoomed: bool, iconic: bool) -> bool {
    !zoomed && !iconic
}

pub fn restore_place(hwnd: HWND, place: Option<super::app_data::WindowPlace>) {
    let Some(place) = place else {
        return;
    };
    unsafe {
        // **最大化・最小化のときは触らない**（RFN01-45）。ここが入れるのは
        // **元の大きさと位置**で、Windowsはそれを自分でも持っている。状態と矩形は
        // 別のものなので、最大化された窓へ入れ直すと「状態は最大化、大きさは通常」
        // という食い違いになる（キャプションは「元の大きさに戻す」の絵なのに、
        // 実際は通常の大きさ、そして戻すとさらに小さくなる）。
        if !wants_place(IsZoomed(hwnd).as_bool(), IsIconic(hwnd).as_bool()) {
            return;
        }
        let mut outer = RECT::default();
        let mut client = RECT::default();
        if GetWindowRect(hwnd, &mut outer).is_err() || GetClientRect(hwnd, &mut client).is_err() {
            return;
        }
        let frame_x = (outer.right - outer.left) - (client.right - client.left);
        let frame_y = (outer.bottom - outer.top) - (client.bottom - client.top);
        let _ = SetWindowPos(
            hwnd,
            None,
            place.x,
            place.y,
            place.width as i32 + frame_x,
            place.height as i32 + frame_y,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveWindowSubclass(self.hwnd, Some(frame_proc), SUBCLASS_ID);
            let _ = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

/// Pure hit testing, also used by tests at negative screen origins after mapping.
fn title_hit(x: f32, y: f32, width: f32, interactive_end: f32) -> u32 {
    if y < 0. || y >= TITLE_HEIGHT || x < 0. || x >= width {
        return HTCLIENT;
    }
    if x >= width - BUTTON_WIDTH {
        HTCLOSE
    } else if x >= width - 2. * BUTTON_WIDTH {
        HTMAXBUTTON
    } else if x >= width - 3. * BUTTON_WIDTH {
        HTMINBUTTON
    } else if x < 32. {
        HTCLIENT // App icon opens the title menu; Alt+Space retains the system menu.
    } else if x < interactive_end {
        HTCLIENT
    } else {
        HTCAPTION
    }
}

/// The caption button a Windows hit-test code stands for. Windows sends its own
/// answer back in `WM_NCMOUSEMOVE`, so the frame learns which button the pointer
/// is over without hit testing the same point a second time.
fn caption_slot(hit: u32) -> u32 {
    match hit {
        HTMINBUTTON => 1,
        HTMAXBUTTON => 2,
        HTCLOSE => 3,
        _ => 0,
    }
}

unsafe extern "system" fn frame_proc(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        if message == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(hwnd, Some(frame_proc), SUBCLASS_ID);
            return DefSubclassProc(hwnd, message, w, l);
        }
        if message == WM_MENUSELECT {
            super::menu_commands::native_selection(w, l);
        }
        let chrome = &*(data as *const Chrome);
        if message == WM_CAPTION_STATE {
            // Posted from the frame callback, so this runs from the message
            // queue and cannot reenter a resize the way a direct call would.
            if let Some(window) = chrome.window.upgrade() {
                window.set_title_hot(w.0 as i32);
                window.set_title_pressed(l.0 != 0);
            }
            return LRESULT(0);
        }
        if message == WM_NCMOUSEMOVE {
            chrome.hot.set(caption_slot(w.0 as u32));
            chrome.publish();
            if chrome.hot.get() != 0 {
                chrome.track_leave();
            }
        }
        if (message == WM_MOUSEMOVE || message == WM_NCMOUSELEAVE) && chrome.hot.get() != 0 {
            // Into the client area (the menus, the icon, the document) or out of
            // the window: either way nothing on the caption is under the pointer.
            chrome.hot.set(0);
            chrome.publish();
        }
        if message == WM_NCLBUTTONDOWN && matches!(w.0 as u32, HTMINBUTTON | HTMAXBUTTON | HTCLOSE)
        {
            chrome.hot.set(caption_slot(w.0 as u32));
            chrome.pressed.set(w.0 as u32);
            chrome.publish();
            SetCapture(hwnd);
            return LRESULT(0);
        }
        if message == WM_LBUTTONUP && chrome.pressed.get() != 0 {
            let pressed = chrome.pressed.replace(0);
            chrome.publish();
            let _ = ReleaseCapture();
            let x = (l.0 as u32 & 0xffff) as i16 as f32;
            let y = ((l.0 as u32 >> 16) & 0xffff) as i16 as f32;
            let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_ok()
                && title_hit(
                    x / scale,
                    y / scale,
                    rect.right as f32 / scale,
                    chrome.interactive_end.get(),
                ) == pressed
            {
                let command = match pressed {
                    HTMINBUTTON => SC_MINIMIZE,
                    HTMAXBUTTON => {
                        if IsZoomed(hwnd).as_bool() {
                            SC_RESTORE
                        } else {
                            SC_MAXIMIZE
                        }
                    }
                    _ => SC_CLOSE,
                };
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_SYSCOMMAND,
                    WPARAM(command as usize),
                    LPARAM(0),
                );
            }
            return LRESULT(0);
        }
        if message == WM_CAPTURECHANGED {
            chrome.pressed.set(0);
            chrome.publish();
        }
        if message == WM_KEYDOWN && w.0 == 0x1b && chrome.pressed.replace(0) != 0 {
            chrome.publish();
            let _ = ReleaseCapture();
            return LRESULT(0);
        }
        if message == WM_NCCALCSIZE && w.0 != 0 {
            // Let Windows calculate the side/bottom borders; replace only its
            // caption inset. Maximized windows still need the invisible top frame.
            let params = &mut *(l.0 as *mut NCCALCSIZE_PARAMS);
            let top = params.rgrc[0].top;
            let _ = DefSubclassProc(hwnd, message, w, l);
            let dpi = GetDpiForWindow(hwnd);
            let frame = GetSystemMetricsForDpi(SM_CYFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            params.rgrc[0].top = top + if IsZoomed(hwnd).as_bool() { frame } else { 0 };
            return LRESULT(0);
        }
        if message == WM_NCHITTEST {
            let mut native = LRESULT(0);
            let handled = DwmDefWindowProc(hwnd, message, w, l, &mut native).as_bool();
            // Keep Windows' outer border tests, but use our caption geometry.
            if handled
                && matches!(
                    native.0 as u32,
                    HTLEFT
                        | HTRIGHT
                        | HTTOP
                        | HTBOTTOM
                        | HTTOPLEFT
                        | HTTOPRIGHT
                        | HTBOTTOMLEFT
                        | HTBOTTOMRIGHT
                )
            {
                return native;
            }
            let standard = DefSubclassProc(hwnd, message, w, l);
            if matches!(
                standard.0 as u32,
                HTLEFT | HTRIGHT | HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT
            ) {
                return standard;
            }
            let mut point = POINT {
                x: (l.0 as u32 & 0xffff) as i16 as i32,
                y: ((l.0 as u32 >> 16) & 0xffff) as i16 as i32,
            };
            if !ScreenToClient(hwnd, &mut point).as_bool() {
                return standard;
            }
            let dpi = GetDpiForWindow(hwnd).max(96);
            let scale = dpi as f32 / 96.;
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_err() {
                return standard;
            }
            let border = GetSystemMetricsForDpi(SM_CYFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            if !IsZoomed(hwnd).as_bool() {
                let left = point.x < border;
                let right = point.x >= rect.right - border;
                let top = point.y < border;
                let bottom = point.y >= rect.bottom - border;
                let hit = match (left, right, top, bottom) {
                    (true, _, true, _) => HTTOPLEFT,
                    (_, true, true, _) => HTTOPRIGHT,
                    (true, _, _, true) => HTBOTTOMLEFT,
                    (_, true, _, true) => HTBOTTOMRIGHT,
                    (_, _, true, _) => HTTOP,
                    (_, _, _, true) => HTBOTTOM,
                    (true, _, _, _) => HTLEFT,
                    (_, true, _, _) => HTRIGHT,
                    _ => HTCLIENT,
                };
                if hit != HTCLIENT {
                    return LRESULT(hit as isize);
                }
            }
            let chrome = &*(data as *const Chrome);
            return LRESULT(title_hit(
                point.x as f32 / scale,
                point.y as f32 / scale,
                rect.right as f32 / scale,
                chrome.interactive_end.get(),
            ) as isize);
        }
        let mut native = LRESULT(0);
        if DwmDefWindowProc(hwnd, message, w, l, &mut native).as_bool() {
            return native;
        }
        DefSubclassProc(hwnd, message, w, l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFN01-45: **最大化中・最小化中の窓には、覚えていた大きさを入れない。**
    /// 入れるのは「元の大きさ」で、Windowsがそれを自分で持っている。
    #[test]
    fn a_maximized_window_keeps_its_own_place() {
        assert!(wants_place(false, false), "通常の窓には入れる");
        assert!(!wants_place(true, false), "最大化中は入れない");
        assert!(!wants_place(false, true), "最小化中は入れない");
    }

    #[test]
    fn menus_do_not_drag_and_caption_buttons_do_not_run_commands() {
        assert_eq!(title_hit(16., 18., 640., 36.), HTCLIENT);
        assert_eq!(title_hit(60., 20., 640., 380.), HTCLIENT);
        assert_eq!(title_hit(410., 20., 640., 380.), HTCAPTION);
        assert_eq!(title_hit(520., 20., 640., 380.), HTMINBUTTON);
        assert_eq!(title_hit(570., 20., 640., 380.), HTMAXBUTTON);
        assert_eq!(title_hit(620., 20., 640., 380.), HTCLOSE);
        assert_eq!(title_hit(60., 36., 640., 380.), HTCLIENT);
        assert_eq!(title_hit(-10., 20., 640., 380.), HTCLIENT);
    }

    /// Only the three caption buttons light up. Windows sends its own hit-test
    /// code in `WM_NCMOUSEMOVE`, so the caption, the icon and the resize border
    /// have to fall through to "nothing".
    #[test]
    fn only_the_caption_buttons_report_a_slot() {
        assert_eq!(caption_slot(HTMINBUTTON), 1);
        assert_eq!(caption_slot(HTMAXBUTTON), 2);
        assert_eq!(caption_slot(HTCLOSE), 3);
        assert_eq!(caption_slot(HTCAPTION), 0);
        assert_eq!(caption_slot(HTCLIENT), 0);
        assert_eq!(caption_slot(HTTOP), 0);
        assert_eq!(caption_slot(HTBOTTOMRIGHT), 0);
    }
}
