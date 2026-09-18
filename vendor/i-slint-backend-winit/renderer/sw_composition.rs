// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0
// RFN Edit: preserve per-pixel alpha and native non-client window decorations.
use super::WinitCompatibleRenderer;
use i_slint_core::{platform::PlatformError, renderer::DrawOutcome};
pub use i_slint_renderer_software::SoftwareRenderer;
use i_slint_renderer_software::{PremultipliedRgbaColor, RepaintBufferType, TargetPixel};
use raw_window_handle::HasWindowHandle;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};
use windows::{
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            DirectComposition::*,
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};
use winit::{event_loop::ActiveEventLoop, platform::windows::WindowAttributesExtWindows};

#[derive(Clone, Copy, Default)]
#[repr(transparent)]
struct Pixel(u32);
impl From<PremultipliedRgbaColor> for Pixel {
    fn from(p: PremultipliedRgbaColor) -> Self {
        Self(
            (u32::from(p.alpha) << 24)
                | (u32::from(p.red) << 16)
                | (u32::from(p.green) << 8)
                | u32::from(p.blue),
        )
    }
}
impl TargetPixel for Pixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let mut p = PremultipliedRgbaColor {
            alpha: (self.0 >> 24) as u8,
            red: (self.0 >> 16) as u8,
            green: (self.0 >> 8) as u8,
            blue: self.0 as u8,
        };
        p.blend(color);
        *self = p.into();
    }
    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(0xff000000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
    }
    fn background() -> Self {
        Self(0)
    }
}

struct Surface {
    // Keep the composition tree alive until the surface is released, before HWND destruction.
    _composition: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    chain: IDXGISwapChain1,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    pixels: Vec<Pixel>,
}
impl Surface {
    fn new(hwnd: HWND, width: u32, height: u32) -> windows::core::Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            // WARP supports machines without an available hardware D3D11 device.
            let create = |kind, device: &mut _, context: &mut _| {
                D3D11CreateDevice(
                    None,
                    kind,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(device),
                    None,
                    Some(context),
                )
            };
            if create(D3D_DRIVER_TYPE_HARDWARE, &mut device, &mut context).is_err() {
                create(D3D_DRIVER_TYPE_WARP, &mut device, &mut context)?;
            }
            let device = device.ok_or_else(windows::core::Error::from_thread)?;
            let context = context.ok_or_else(windows::core::Error::from_thread)?;
            let dxgi: IDXGIDevice = device.cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let chain = factory.CreateSwapChainForComposition(
                &device,
                &DXGI_SWAP_CHAIN_DESC1 {
                    Width: width,
                    Height: height,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                    ..Default::default()
                },
                None,
            )?;
            let composition: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let target = composition.CreateTargetForHwnd(hwnd, false)?;
            let visual = composition.CreateVisual()?;
            visual.SetContent(&chain)?;
            target.SetRoot(&visual)?;
            composition.Commit()?;
            Ok(Self {
                _composition: composition,
                _target: target,
                _visual: visual,
                chain,
                context,
                width,
                height,
                pixels: vec![Pixel::default(); width as usize * height as usize],
            })
        }
    }
    fn resize(&mut self, width: u32, height: u32) -> windows::core::Result<()> {
        unsafe {
            self.chain.ResizeBuffers(
                2,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.width = width;
        self.height = height;
        self.pixels
            .resize(width as usize * height as usize, Pixel::default());
        Ok(())
    }
    fn present(&self) -> windows::core::Result<()> {
        unsafe {
            let texture: ID3D11Texture2D = self.chain.GetBuffer(0)?;
            // Upload the complete image: the flip-chain back buffer can be two frames old.
            self.context.UpdateSubresource(
                &texture,
                0,
                None,
                self.pixels.as_ptr().cast(),
                self.width * 4,
                0,
            );
            self.chain.Present(0, DXGI_PRESENT(0)).ok()
        }
    }
}

pub struct WinitSoftwareRenderer {
    renderer: SoftwareRenderer,
    surface: RefCell<Option<Surface>>,
    window: RefCell<Option<Arc<winit::window::Window>>>,
    repaint: Cell<bool>,
}
impl WinitSoftwareRenderer {
    pub fn new_suspended(
        _: &Rc<crate::SharedBackendData>,
    ) -> Result<Box<dyn WinitCompatibleRenderer>, PlatformError> {
        Ok(Box::new(Self {
            renderer: SoftwareRenderer::new(),
            surface: RefCell::new(None),
            window: RefCell::new(None),
            repaint: Cell::new(true),
        }))
    }
    fn draw(&self, hwnd: HWND, width: u32, height: u32) -> windows::core::Result<()> {
        let mut surface = self.surface.borrow_mut();
        if surface.is_none() {
            *surface = Some(Surface::new(hwnd, width, height)?);
            self.repaint.set(true);
        }
        let surface = surface.as_mut().unwrap();
        if (surface.width, surface.height) != (width, height) {
            surface.resize(width, height)?;
            self.repaint.set(true);
        }
        let full = self.repaint.replace(false);
        self.renderer.set_repaint_buffer_type(if full {
            RepaintBufferType::NewBuffer
        } else {
            RepaintBufferType::ReusedBuffer
        });
        let region = self.renderer.render(&mut surface.pixels, width as usize);
        if full || region.bounding_box_size().width > 0 {
            surface.present()?;
        }
        Ok(())
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
        let hwnd = match native.window_handle().map_err(|e| e.to_string())?.as_raw() {
            raw_window_handle::RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as _),
            _ => return Err("Expected a Win32 window".into()),
        };
        if let Err(error) = self.draw(hwnd, size.width, size.height) {
            if error.code() == DXGI_ERROR_DEVICE_REMOVED || error.code() == DXGI_ERROR_DEVICE_RESET
            {
                self.surface.borrow_mut().take();
                self.draw(hwnd, size.width, size.height)
                    .map_err(|e| format!("DirectComposition device recovery failed: {e}"))?;
            } else {
                return Err(format!("DirectComposition presentation failed: {error}").into());
            }
        }
        Ok(DrawOutcome::Success)
    }
    fn as_core_renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &self.renderer
    }
    fn occluded(&self, _: bool) {
        self.repaint.set(true);
    }
    fn resume(
        &self,
        active: &ActiveEventLoop,
        attrs: winit::window::WindowAttributes,
    ) -> Result<Arc<winit::window::Window>, PlatformError> {
        let window = Arc::new(
            active
                .create_window(attrs.with_no_redirection_bitmap(true))
                .map_err(|e| e.to_string())?,
        );
        *self.window.borrow_mut() = Some(window.clone());
        self.repaint.set(true);
        Ok(window)
    }
    fn suspend(&self) -> Result<(), PlatformError> {
        self.surface.borrow_mut().take();
        self.window.borrow_mut().take();
        Ok(())
    }
}
