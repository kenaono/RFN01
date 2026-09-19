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

## Accessibility focus reentrancy

`winitwindowadapter.rs::handle_focus_change` uses `try_borrow_mut` and schedules
busy notifications on the next Slint timer turn through a weak window reference.
AccessKit `reload_tree` / `focus_node` can instantiate a lazy Field whose init
handler changes focus while the adapter is borrowed. An unconditional mutable
borrow aborts the native callback in that situation (reproduced from the
Workspace context menu's Clone command with accessibility active).

The normal unborrowed path remains synchronous. A deferred notification reads
current focus rather than retaining an obsolete item, retries if still busy, and
does nothing after the window has been dropped. Accessibility stays enabled;
focus notifications are deferred rather than silently discarded.

On upgrade, check whether upstream handles this reentrancy and remove this patch
if superseded. Verify opening, cancelling, and reopening lazy input dialogs from
menus with accessibility active, plus keyboard focus and screen-reader focus.
