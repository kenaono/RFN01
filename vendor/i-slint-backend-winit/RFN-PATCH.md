# RFN Edit Windows transparency patch

Based on the crates.io release `i-slint-backend-winit 1.18.1` (rebased from
1.17.1 on 2026-11-03).
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

`renderer/sw_softbuffer.rs` is upstream's `renderer/sw.rs` of the same release,
unchanged: copy it over on each upgrade. The other renderer files follow the
upstream `WinitCompatibleRenderer` trait; 1.18 added the window adapter's weak
reference to `resume`, which `sw_composition.rs` takes and does not use.

How the 1.17.1 → 1.18.1 rebase was done: a three-way merge per file (base =
upstream 1.17.1, ours = this folder in Git, LF line endings; theirs = upstream
1.18.1). `lib.rs` and `Cargo.toml` merged cleanly. `winitwindowadapter.rs` had
two conflicts: the window-created hook now runs after upstream's new
`first_frame_presented` reset, and upstream's own `try_borrow_mut` in
`handle_focus_change` still drops a busy notification, so the deferral below is
kept.

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

## Partial upload on present

`sw_composition.rs::Surface::present` uploads only the rectangle that differs
from the back buffer instead of the whole image. The flip-sequential back buffer
holds the frame from two presents ago, so the upload is the bounding box of this
frame's dirty region and the previous presented one; after creation or
`ResizeBuffers` both buffers are filled in full (`stale`). A caret blink went
from 1.3–1.7ms to under 1ms at 2219×1726 (2026-09-23).

## Window-created hook

`lib.rs::set_window_created_hook` registers a closure that `ensure_window` runs on
each new winit window right after it is created and before it is first shown
(Slint creates windows hidden and shows them after pre-rendering the first
frame). RFN Edit installs its title bar, restores the window place and reports
the resulting client size there, so the first visible frame already has its
final frame, place and layout (2026-09-23).
