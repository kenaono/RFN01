// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0
// Experimental replacement for Slint 1.17.1 renderer/sw.rs, Windows only.
// Keeps winit's event/IME handling; changes only the software presentation path.
use super::WinitCompatibleRenderer;
use i_slint_core::{platform::PlatformError, renderer::DrawOutcome};
pub use i_slint_renderer_software::SoftwareRenderer;
use i_slint_renderer_software::{PremultipliedRgbaColor, RepaintBufferType};
use raw_window_handle::HasWindowHandle;
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};
use windows::Win32::{
    Foundation::{COLORREF, HWND, POINT, SIZE},
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::*,
};
use winit::event_loop::ActiveEventLoop;

fn log(message: String) {
    use std::io::Write;
    eprintln!("{message}");
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(exe.with_extension("log"))
        {
            let _ = writeln!(file, "{message}");
        }
    }
}

pub struct WinitSoftwareRenderer {
    renderer: SoftwareRenderer,
    window: RefCell<Option<Arc<winit::window::Window>>>,
    bitmap: RefCell<Option<Bitmap>>,
    stats: RefCell<Vec<f64>>,
}

// A top-down, premultiplied BGRA DIB. Restore the old GDI object before deletion.
struct Bitmap {
    dc: HDC,
    handle: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    width: u32,
    height: u32,
    rgba: Vec<PremultipliedRgbaColor>,
}
impl Bitmap {
    fn new(width: u32, height: u32) -> Result<Self, PlatformError> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.0.is_null() {
                return Err("CreateCompatibleDC failed".into());
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let handle = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(handle) => handle,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(format!("CreateDIBSection: {e}").into());
                }
            };
            let old = SelectObject(dc, HGDIOBJ(handle.0));
            if old.0.is_null() || old.0 as isize == -1 {
                let _ = DeleteObject(HGDIOBJ(handle.0));
                let _ = DeleteDC(dc);
                return Err("SelectObject failed".into());
            }
            Ok(Self {
                dc,
                handle,
                old,
                bits: bits.cast(),
                width,
                height,
                rgba: vec![PremultipliedRgbaColor::default(); width as usize * height as usize],
            })
        }
    }
}
impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.handle.0));
            let _ = DeleteDC(self.dc);
        }
    }
}
fn hwnd(window: &winit::window::Window) -> Result<HWND, PlatformError> {
    match window.window_handle().map_err(|e| e.to_string())?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as _)),
        _ => Err("This probe requires Windows".into()),
    }
}
impl WinitSoftwareRenderer {
    pub fn new_suspended(
        _: &Rc<crate::SharedBackendData>,
    ) -> Result<Box<dyn WinitCompatibleRenderer>, PlatformError> {
        Ok(Box::new(Self {
            renderer: SoftwareRenderer::new(),
            window: RefCell::new(None),
            bitmap: RefCell::new(None),
            stats: RefCell::new(Vec::new()),
        }))
    }
}
impl WinitCompatibleRenderer for WinitSoftwareRenderer {
    fn render(&self, window: &i_slint_core::api::Window) -> Result<DrawOutcome, PlatformError> {
        let size = window.size();
        if size.width == 0 || size.height == 0 {
            return Ok(DrawOutcome::Success);
        }
        let native = self.window.borrow();
        let Some(native) = native.as_ref() else {
            return Ok(DrawOutcome::Success);
        };
        let start = Instant::now();
        let mut stored = self.bitmap.borrow_mut();
        let resized = stored
            .as_ref()
            .is_none_or(|b| (b.width, b.height) != (size.width, size.height));
        if resized {
            *stored = Some(Bitmap::new(size.width, size.height)?);
        }
        let bitmap = stored.as_mut().unwrap();
        self.renderer.set_repaint_buffer_type(if resized {
            RepaintBufferType::NewBuffer
        } else {
            RepaintBufferType::ReusedBuffer
        });
        self.renderer.render(&mut bitmap.rgba, size.width as usize);
        unsafe {
            // winit reapplies its own styles on show/resize; preserve our presentation flag.
            let handle = hwnd(native)?;
            let style = GetWindowLongPtrW(handle, GWL_EXSTYLE);
            if style & WS_EX_LAYERED.0 as isize == 0 {
                SetWindowLongPtrW(handle, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
            }
            let pixels = std::slice::from_raw_parts_mut(bitmap.bits, bitmap.rgba.len());
            for (dst, p) in pixels.iter_mut().zip(&bitmap.rgba) {
                *dst = ((p.alpha as u32) << 24)
                    | ((p.red as u32) << 16)
                    | ((p.green as u32) << 8)
                    | p.blue as u32;
            }
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let dimensions = SIZE {
                cx: size.width as i32,
                cy: size.height as i32,
            };
            let origin = POINT::default();
            UpdateLayeredWindow(
                hwnd(native)?,
                None,
                None,
                Some(&dimensions),
                Some(bitmap.dc),
                Some(&origin),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .map_err(|e| {
                format!(
                    "UpdateLayeredWindow {}x{} style={:#x}: {e}",
                    size.width,
                    size.height,
                    GetWindowLongPtrW(handle, GWL_EXSTYLE)
                )
            })?;
        }
        let elapsed = start.elapsed().as_secs_f64() * 1000.;
        let mut stats = self.stats.borrow_mut();
        stats.push(elapsed);
        if resized {
            let semi = bitmap
                .rgba
                .iter()
                .filter(|p| p.alpha > 0 && p.alpha < 255)
                .count();
            let opaque = bitmap.rgba.iter().filter(|p| p.alpha == 255).count();
            log(format!(
                "LAYERED {}x{} semi={} opaque={} first_ms={:.2}",
                size.width, size.height, semi, opaque, elapsed
            ));
        }
        if stats.len() == 60 {
            stats.sort_by(f64::total_cmp);
            log(format!(
                "FRAME {}x{} n=60 p50_ms={:.2} p95_ms={:.2} max_ms={:.2}",
                size.width, size.height, stats[30], stats[57], stats[59]
            ));
            stats.clear();
        }
        Ok(DrawOutcome::Success)
    }
    fn as_core_renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &self.renderer
    }
    fn occluded(&self, _: bool) {
        // Discard the DIB so the next render refreshes the whole backing buffer.
        self.bitmap.borrow_mut().take();
    }
    fn resume(
        &self,
        active: &ActiveEventLoop,
        attrs: winit::window::WindowAttributes,
    ) -> Result<Arc<winit::window::Window>, PlatformError> {
        let window = Arc::new(active.create_window(attrs).map_err(|e| e.to_string())?);
        unsafe {
            let handle = hwnd(&window)?;
            let style = GetWindowLongPtrW(handle, GWL_EXSTYLE);
            SetWindowLongPtrW(handle, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
            if GetWindowLongPtrW(handle, GWL_EXSTYLE) & WS_EX_LAYERED.0 as isize == 0 {
                return Err("Unable to enable WS_EX_LAYERED".into());
            }
        }
        *self.window.borrow_mut() = Some(window.clone());
        Ok(window)
    }
    fn suspend(&self) -> Result<(), PlatformError> {
        self.bitmap.borrow_mut().take();
        self.window.borrow_mut().take();
        Ok(())
    }
}
