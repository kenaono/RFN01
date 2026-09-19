//! Native input policy shared by editor views, TABs and auxiliary windows.
use std::time::Duration;

pub(crate) fn shift_really_held() -> (bool, bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState, VK_SHIFT};

    // SAFETY: どちらも仮想キーの番号を1つ渡すだけの呼び出しで、返るのは状態の
    // 語である。最上位のビットが立っていれば押されている＝符号付きで見れば負。
    let by_message = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
    let by_hand = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
    (by_message, by_hand)
}

pub(crate) fn double_click_time() -> Duration {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;

    // SAFETY: 引数の無い呼び出しで、返るのはミリ秒の数である。失敗しない。
    let ms = unsafe { GetDoubleClickTime() };
    Duration::from_millis(u64::from(ms.max(1)))
}

/// IME can swallow Shift release. Repair Slint's state before its built-in
/// TextInput processes selection, not merely in our shortcut callback.
pub(crate) fn install_shift_repair(window: &slint::Window) {
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
    window.on_winit_window_event(|window, event| {
        if matches!(
            event,
            WindowEvent::MouseInput { .. } | WindowEvent::KeyboardInput { .. }
        ) {
            let (by_message, by_hand) = shift_really_held();
            repair_shift_state(window, by_message, by_hand);
        }
        EventResult::Propagate
    });
}

pub(crate) fn repair_shift_state(window: &slint::Window, by_message: bool, by_hand: bool) {
    if by_message || by_hand {
        return;
    }
    // Release both sides, then let the original event run. No text editing or
    // selection operation is synthesized.
    for key in [slint::platform::Key::Shift, slint::platform::Key::ShiftR] {
        window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: key.into() });
    }
}
