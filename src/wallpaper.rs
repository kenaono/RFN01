//! 背景の壁紙（追加要件 2026-09-15、書き手）。
//!
//! **紙の後ろに画像を敷き、紙を濃さぶん透かす。**敷く画像は2通り：
//!
//! - **Windowsの壁紙**——画面上の同じ位置に敷くので、窓を通してデスクトップが
//!   見えているように感じる（書き手：「Windowsの壁紙を透けて見えるようにして、
//!   なんとなく透明になっている感じ」）。窓そのものを透かすことは、この描画
//!   （femtovg）では画素ごとにはできなかった（技術検証 6.40）。
//! - **指定の画像**——タイル、縦に合わせる、横に合わせる。
//!
//! 画像は面（ペイン）の側がSlintで敷く。ここは**何をどこに敷くか**を決めて窓へ
//! 置くだけで、描くことはしない。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use slint::{Color, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use windows::Win32::Foundation::{GENERIC_READ, POINT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand, WICRect,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree,
};
use windows::Win32::UI::Shell::{
    DESKTOP_WALLPAPER_POSITION, DWPOS_CENTER, DWPOS_FILL, DWPOS_FIT, DWPOS_SPAN, DWPOS_STRETCH,
    DWPOS_TILE, DesktopWallpaper, IDesktopWallpaper,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::{AppWindow, WallPiece};

/// 何を敷くか。設定ファイルの`wallpaper.kind`。
pub const NONE: i32 = 0;
pub const DESKTOP: i32 = 1;
pub const FILE: i32 = 2;

/// 画面上の矩形（物理画素）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Windowsの壁紙の「配置」。設定アプリの「デスクトップ画像に合うものを選択」と同じ6つ。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fit {
    Center,
    Tile,
    Stretch,
    Fit,
    Fill,
    Span,
}

impl Fit {
    fn from_windows(position: DESKTOP_WALLPAPER_POSITION) -> Self {
        match position {
            DWPOS_CENTER => Fit::Center,
            DWPOS_TILE => Fit::Tile,
            DWPOS_STRETCH => Fit::Stretch,
            DWPOS_FIT => Fit::Fit,
            DWPOS_SPAN => Fit::Span,
            // 既定の「ページ幅に合わせる」。知らない値もこれに倒す。
            DWPOS_FILL | _ => Fit::Fill,
        }
    }
}

/// 画像を1枚の画面（`Span`なら全画面を囲む矩形）へ置くと、どこに来るか。
///
/// **Windowsと同じ置き方**：合わせる（`Fit`）は縦横比を保って収め、ページ幅
/// （`Fill`）は縦横比を保って覆い、どちらも真ん中に置く。並べる（`Tile`）は画面を
/// 覆う矩形を返し、画像の元の大きさで並べるのは面の側。
pub fn place(fit: Fit, screen: Rect, image: (u32, u32)) -> Rect {
    let (image_width, image_height) = (image.0.max(1) as f64, image.1.max(1) as f64);
    let (width, height) = (screen.width as f64, screen.height as f64);
    let scaled = |scale: f64| {
        let (w, h) = (image_width * scale, image_height * scale);
        Rect {
            x: screen.x + ((width - w) / 2.0).round() as i32,
            y: screen.y + ((height - h) / 2.0).round() as i32,
            width: w.round() as i32,
            height: h.round() as i32,
        }
    };
    match fit {
        Fit::Tile | Fit::Stretch => screen,
        Fit::Center => scaled(1.0),
        Fit::Fit => scaled((width / image_width).min(height / image_height)),
        Fit::Fill | Fit::Span => scaled((width / image_width).max(height / image_height)),
    }
}

/// 読み込んだ画像。**同じファイルは1度だけ読む**——2台の画面が同じ壁紙を
/// 使っていても、4Kなら1枚33MBある。
struct Loaded {
    stamp: Option<(SystemTime, u64)>,
    image: Image,
    size: (u32, u32),
}

thread_local! {
    static LOADED: RefCell<HashMap<PathBuf, Loaded>> = RefCell::new(HashMap::new());
    /// いま窓に置いてあるものの署名。見回りは、これと違うときだけ置き直す。
    static SHOWN: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn stamp_of(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// 画像ファイルを読む（WIC：JPEG・PNG・BMP・GIF・TIFF・JPEG XRなど、Windowsが読めるもの）。
fn decode(path: &Path) -> windows::core::Result<(u32, u32, Vec<u8>)> {
    // SAFETY: COMは窓のスレッドで初期化済み（ファイルダイアログと同じ道）。
    // 渡す矩形とバッファは、読んだ画像の大きさから作っている。
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let name = HSTRING::from(path.as_os_str());
        let decoder = factory.CreateDecoderFromFilename(
            &name,
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppRGBA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut width, mut height) = (0, 0);
        converter.GetSize(&mut width, &mut height)?;
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let rect = WICRect {
            X: 0,
            Y: 0,
            Width: width as i32,
            Height: height as i32,
        };
        converter.CopyPixels(&rect, width * 4, &mut pixels)?;
        Ok((width, height, pixels))
    }
}

/// 画像を読む。前に読んだものと更新時刻・長さが同じなら、読み直さない。
fn load(path: &Path) -> Result<(Image, (u32, u32)), String> {
    let stamp = stamp_of(path);
    let held = LOADED.with(|loaded| {
        loaded
            .borrow()
            .get(path)
            .filter(|held| held.stamp == stamp)
            .map(|held| (held.image.clone(), held.size))
    });
    if let Some(found) = held {
        return Ok(found);
    }
    let (width, height, bytes) = decode(path).map_err(|error| error.message())?;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(&bytes);
    let image = Image::from_rgba8(buffer);
    LOADED.with(|loaded| {
        loaded.borrow_mut().insert(
            path.to_path_buf(),
            Loaded {
                stamp,
                image: image.clone(),
                size: (width, height),
            },
        );
    });
    Ok((image, (width, height)))
}

/// 1台の画面と、そこに出ている壁紙。
struct Monitor {
    rect: Rect,
    file: Option<PathBuf>,
}

/// Windowsの壁紙の、いまの姿：画面ごとの画像、配置、地の色。
struct Desktop {
    monitors: Vec<Monitor>,
    fit: Fit,
    colour: [u8; 3],
}

/// シェルが返した文字列を読んで、解放する。
///
/// SAFETY: `text`はシェルが`CoTaskMemAlloc`で渡したもの。
unsafe fn take_string(text: PWSTR) -> String {
    let read = unsafe { text.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(text.0 as *const _)) };
    read
}

fn desktop() -> windows::core::Result<Desktop> {
    // SAFETY: COMは窓のスレッドで初期化済み。文字列は`take_string`が解放する。
    unsafe {
        let wallpaper: IDesktopWallpaper = CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)?;
        let count = wallpaper.GetMonitorDevicePathCount()?;
        let fit = Fit::from_windows(wallpaper.GetPosition().unwrap_or(DWPOS_FILL));
        let colour = wallpaper.GetBackgroundColor().map(|c| c.0).unwrap_or(0);
        let mut monitors = Vec::new();
        for index in 0..count {
            let Ok(id) = wallpaper.GetMonitorDevicePathAt(index) else {
                continue;
            };
            let rect = wallpaper.GetMonitorRECT(PCWSTR(id.0));
            let file = wallpaper
                .GetWallpaper(PCWSTR(id.0))
                .map(|text| take_string(text))
                .ok()
                .filter(|name| !name.is_empty())
                .map(PathBuf::from);
            take_string(id);
            // **切り離された画面は大きさが0で返る。**そこに敷くものは無い。
            if let Ok(rect) = rect
                && rect.right > rect.left
                && rect.bottom > rect.top
            {
                monitors.push(Monitor {
                    rect: Rect {
                        x: rect.left,
                        y: rect.top,
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                    },
                    file,
                });
            }
        }
        // COLORREF は 0x00BBGGRR。
        let rgb = [
            (colour & 0xff) as u8,
            ((colour >> 8) & 0xff) as u8,
            ((colour >> 16) & 0xff) as u8,
        ];
        Ok(Desktop {
            monitors,
            fit,
            colour: rgb,
        })
    }
}

/// 全部の画面を囲む矩形（`Span`の置き場所）。
fn union(monitors: &[Monitor]) -> Rect {
    let left = monitors.iter().map(|m| m.rect.x).min().unwrap_or(0);
    let top = monitors.iter().map(|m| m.rect.y).min().unwrap_or(0);
    let right = monitors
        .iter()
        .map(|m| m.rect.x + m.rect.width)
        .max()
        .unwrap_or(0);
    let bottom = monitors
        .iter()
        .map(|m| m.rect.y + m.rect.height)
        .max()
        .unwrap_or(0);
    Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// 1台の画面ぶんの片。**画面の矩形で切り**、その中に地の色と画像を置く——
/// 中央やページ幅で画面より大きくなった画像が、隣の画面の上へ出ないように。
fn piece(
    monitor: Rect,
    image: Rect,
    scale: f32,
    source: Option<Image>,
    tiled: bool,
    colour: Color,
) -> WallPiece {
    let logical = |value: i32| value as f32 / scale;
    WallPiece {
        x: logical(monitor.x),
        y: logical(monitor.y),
        width: logical(monitor.width),
        height: logical(monitor.height),
        image_x: logical(image.x - monitor.x),
        image_y: logical(image.y - monitor.y),
        image_width: logical(image.width),
        image_height: logical(image.height),
        has_image: source.is_some(),
        source: source.unwrap_or_default(),
        tiled,
        colour,
    }
}

/// いまの設定の署名。**読まずに作れるものだけ**（パス・更新時刻・画面の並び）。
fn signature(window: &AppWindow) -> String {
    match window.get_wall_kind() {
        DESKTOP => match desktop() {
            Ok(found) => {
                let monitors = found
                    .monitors
                    .iter()
                    .map(|m| {
                        let stamp = m.file.as_deref().and_then(stamp_of);
                        format!("{:?}{:?}{:?}", m.rect, m.file, stamp)
                    })
                    .collect::<String>();
                format!(
                    "desktop {:?} {:?} {monitors} {}",
                    found.fit,
                    found.colour,
                    window.window().scale_factor()
                )
            }
            Err(error) => format!("desktop error {error}"),
        },
        FILE => {
            let path = PathBuf::from(window.get_wall_path().as_str());
            format!("file {path:?} {:?}", stamp_of(&path))
        }
        _ => "none".to_owned(),
    }
}

/// 設定どおりの壁紙を窓へ置く。**読めなかったら理由を返す**（面は紙だけになる）。
pub fn publish(window: &AppWindow) -> Result<(), String> {
    SHOWN.with(|shown| *shown.borrow_mut() = Some(signature(window)));
    let kind = window.get_wall_kind();
    let mut pieces = Vec::new();
    let mut result = Ok(());
    let mut file_image = Image::default();
    let mut file_size = (0, 0);
    match kind {
        DESKTOP => match desktop() {
            Ok(found) => {
                let scale = window.window().scale_factor().max(0.1);
                let ground = Color::from_rgb_u8(found.colour[0], found.colour[1], found.colour[2]);
                // `Span`は全部の画面を囲む矩形へ1枚を置き、各画面がその自分の分を見せる。
                let spanned = union(&found.monitors);
                for monitor in &found.monitors {
                    let screen = if found.fit == Fit::Span {
                        spanned
                    } else {
                        monitor.rect
                    };
                    let loaded = match &monitor.file {
                        Some(file) => match load(file) {
                            Ok(found) => Some(found),
                            Err(error) => {
                                result = Err(format!("{}: {error}", file.display()));
                                None
                            }
                        },
                        None => None,
                    };
                    let (source, placed) = match loaded {
                        Some((image, size)) => (Some(image), place(found.fit, screen, size)),
                        None => (None, monitor.rect),
                    };
                    pieces.push(piece(
                        monitor.rect,
                        placed,
                        scale,
                        source,
                        found.fit == Fit::Tile,
                        ground,
                    ));
                }
            }
            Err(error) => result = Err(error.message()),
        },
        FILE => {
            let path = PathBuf::from(window.get_wall_path().as_str());
            if path.as_os_str().is_empty() {
                result = Err("画像ファイルが選ばれていません".to_owned());
            } else {
                match load(&path) {
                    Ok((image, size)) => {
                        file_image = image;
                        file_size = size;
                    }
                    Err(error) => result = Err(format!("{}: {error}", path.display())),
                }
            }
        }
        _ => {}
    }
    // **使わなくなった画像は手放す**——4Kの壁紙を切り替えるたびに33MBずつ溜まる。
    let kept: Vec<PathBuf> = match kind {
        DESKTOP => desktop()
            .map(|found| found.monitors.into_iter().filter_map(|m| m.file).collect())
            .unwrap_or_default(),
        FILE => vec![PathBuf::from(window.get_wall_path().as_str())],
        _ => Vec::new(),
    };
    LOADED.with(|loaded| loaded.borrow_mut().retain(|path, _| kept.contains(path)));
    window.set_wall_pieces(ModelRc::from(Rc::new(VecModel::from(pieces))));
    window.set_wall_image(file_image);
    window.set_wall_image_width(file_size.0 as f32);
    window.set_wall_image_height(file_size.1 as f32);
    follow_window(window);
    result
}

/// 見回り（2秒ごと）：壁紙が替わっていたら置き直す。**替わっていなければ何もしない。**
///
/// 返すのは、置き直してうまくいかなかったときの理由。
pub fn refresh_if_changed(window: &AppWindow) -> Option<String> {
    if window.get_wall_kind() == NONE {
        return None;
    }
    let now = signature(window);
    let same = SHOWN.with(|shown| shown.borrow().as_deref() == Some(now.as_str()));
    if same {
        return None;
    }
    publish(window).err()
}

/// 窓の中の原点が、画面のどこにあるか（論理画素）。**窓が動くたびに**呼ぶ——
/// Windowsの壁紙は画面に固定されているので、窓が動けば敷く位置が逆へ動く。
pub fn follow_window(window: &AppWindow) {
    if window.get_wall_kind() != DESKTOP {
        return;
    }
    let Some(hwnd) = crate::ime::window_handle(window) else {
        return;
    };
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: 窓のハンドルは生きている窓のもの。
    if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
        return;
    }
    let scale = window.window().scale_factor().max(0.1);
    window.set_wall_origin_x(origin.x as f32 / scale);
    window.set_wall_origin_y(origin.y as f32 / scale);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect {
        x: 3840,
        y: 0,
        width: 3840,
        height: 2160,
    };

    #[test]
    fn the_six_fits_place_like_windows() {
        // 横長の画面へ、縦長の画像（1000×2000）。
        let image = (1000, 2000);
        assert_eq!(place(Fit::Stretch, SCREEN, image), SCREEN);
        assert_eq!(place(Fit::Tile, SCREEN, image), SCREEN);
        // 合わせる：高さに合わせて1080×2160、左右は真ん中。
        assert_eq!(
            place(Fit::Fit, SCREEN, image),
            Rect {
                x: 3840 + 1380,
                y: 0,
                width: 1080,
                height: 2160
            }
        );
        // ページ幅：幅に合わせて3840×7680、上下は真ん中ではみ出す。
        assert_eq!(
            place(Fit::Fill, SCREEN, image),
            Rect {
                x: 3840,
                y: -2760,
                width: 3840,
                height: 7680
            }
        );
        // 中央：元の大きさのまま真ん中。
        assert_eq!(
            place(Fit::Center, SCREEN, image),
            Rect {
                x: 3840 + 1420,
                y: 80,
                width: 1000,
                height: 2000
            }
        );
    }
}
