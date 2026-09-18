# RFN Edit Windows transparency patch

Based on the cached crates.io release `i-slint-backend-winit 1.17.1`.
Original source and license notices are retained.

Changes: Windows features in Cargo.toml; renderer/sw.rs dispatches to
sw_composition.rs on Windows and the unchanged sw_softbuffer.rs elsewhere.
The Windows software renderer uploads premultiplied BGRA to a DirectComposition
swap chain. WS_EX_NOREDIRECTIONBITMAP leaves client-area alpha available to DWM
while preserving the native title bar, hit testing, resizing and winit IME.
Hardware D3D11 creation falls back to WARP; removed/reset devices are recreated once.
The original OpenGL renderer is unchanged.

When upgrading Slint, rebase this small presentation patch and re-run native
transparency, menu, IME, resize/maximize and long-document checks.
