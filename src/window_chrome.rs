//! Title-row client content with native Windows move/resize/caption commands.
//! Never call Slint while servicing a native frame callback: resizing can reenter.
use std::cell::Cell;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Dwm::DwmDefWindowProc,
    Graphics::Gdi::ScreenToClient,
    UI::{
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const SUBCLASS_ID: usize = 0x52464e41;
pub const TITLE_HEIGHT: f32 = 36.;
pub const BUTTON_WIDTH: f32 = 46.;

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
    // UI geometry, logical pixels. Only accessed on the window's owning thread.
    interactive_end: Cell<f32>,
    pressed: Cell<u32>,
}

impl Chrome {
    pub fn install(hwnd: HWND) -> Result<Box<Self>, windows::core::Error> {
        let chrome = Box::new(Self {
            hwnd,
            interactive_end: Cell::new(36.),
            pressed: Cell::new(0),
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
        HTSYSMENU
    } else if x < interactive_end {
        HTCLIENT
    } else {
        HTCAPTION
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
        if message == WM_NCLBUTTONDOWN && matches!(w.0 as u32, HTMINBUTTON | HTMAXBUTTON | HTCLOSE)
        {
            chrome.pressed.set(w.0 as u32);
            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
            return LRESULT(0);
        }
        if message == WM_LBUTTONUP && chrome.pressed.get() != 0 {
            let pressed = chrome.pressed.replace(0);
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
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
        }
        if message == WM_KEYDOWN && w.0 == 0x1b && chrome.pressed.replace(0) != 0 {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
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
    #[test]
    fn menus_do_not_drag_and_caption_buttons_do_not_run_commands() {
        assert_eq!(title_hit(60., 20., 640., 380.), HTCLIENT);
        assert_eq!(title_hit(410., 20., 640., 380.), HTCAPTION);
        assert_eq!(title_hit(520., 20., 640., 380.), HTMINBUTTON);
        assert_eq!(title_hit(570., 20., 640., 380.), HTMAXBUTTON);
        assert_eq!(title_hit(620., 20., 640., 380.), HTCLOSE);
        assert_eq!(title_hit(60., 36., 640., 380.), HTCLIENT);
        assert_eq!(title_hit(-10., 20., 640., 380.), HTCLIENT);
    }
}
