// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0
use super::WinitCompatibleRenderer;
#[cfg(target_os = "windows")]
#[path = "sw_composition.rs"]
mod implementation;
#[cfg(not(target_os = "windows"))]
#[path = "sw_softbuffer.rs"]
mod implementation;
pub use implementation::*;
