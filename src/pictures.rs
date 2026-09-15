//! 本文の画像（追加要件 2026-09-15、書き手）。
//!
//! **画像だけの行を、ライブプレビューで絵として出す。**ここは行き先をファイルへ解決し、読んで、
//! 描く大きさを決めるところまで。箱を立てるのは`document`、描くのは`directwrite_render`。
//!
//! - 行き先は**文書の置き場所から**読む（`![](img/a.png)`は文書のフォルダの`img/a.png`）。
//!   `%20`などの百分率符号は戻す（Obsidianが書き出す形）。
//! - 大きさは**元の画素数を、表示倍率ぶん**。`|300`があれば幅300に合わせて縦横比を保つ。
//!   行に入らなければ組む側が行の長さへ縮める。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::text_blocks::Picture;

/// 読んだ絵。**読めなかったことも持つ**——打鍵のたびに組み直すので、無いファイルを毎回開きに
/// 行かない。更新時刻か長さが変われば読み直す。
struct Loaded {
    stamp: Option<(SystemTime, u64)>,
    picture: Option<Arc<Picture>>,
}

thread_local! {
    static LOADED: RefCell<HashMap<PathBuf, Loaded>> = RefCell::new(HashMap::new());
    /// WICはCOMの上にある。**組版の前に絵を読むことがある**（面の最初の組み立ては、描く道具より
    /// 先に本文を用意する）ので、このスレッドの間ずっと持つ。
    static APARTMENT: Option<crate::directwrite_render::ComApartment> =
        crate::directwrite_render::ensure_com_apartment().ok().flatten();
}

fn decode(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    APARTMENT.with(|_| ());
    crate::wallpaper::decode(path).ok()
}

fn stamp_of(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// `%20`のような百分率符号を戻す。戻せない並びはそのまま残す。
fn percent_decoded(target: &str) -> String {
    let bytes = target.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| target.to_string())
}

/// 行き先をファイルへ。絶対パスはそのまま、相対パスは文書のフォルダから。**名前の無い文書**
/// （フォルダが無い）では相対パスを解決しない。
pub fn resolve(folder: Option<&Path>, target: &str) -> Option<PathBuf> {
    let decoded = percent_decoded(target);
    let path = Path::new(&decoded);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    Some(folder?.join(path))
}

/// 絵を読む。前に読んだものと更新時刻・長さが同じなら、読み直さない。
pub fn load(path: &Path) -> Option<Arc<Picture>> {
    let stamp = stamp_of(path);
    let held = LOADED.with(|loaded| {
        loaded
            .borrow()
            .get(path)
            .filter(|held| held.stamp == stamp)
            .map(|held| held.picture.clone())
    });
    if let Some(picture) = held {
        return picture;
    }
    let picture = stamp
        .and_then(|_| decode(path))
        .map(|(width, height, rgba)| {
            Arc::new(Picture {
                width,
                height,
                bgra: premultiplied_bgra(rgba),
            })
        });
    LOADED.with(|loaded| {
        loaded.borrow_mut().insert(
            path.to_path_buf(),
            Loaded {
                stamp,
                picture: picture.clone(),
            },
        );
    });
    picture
}

/// WICのRGBA（まっすぐのアルファ）を、Direct2Dが受け取る前掛けのBGRAへ。
fn premultiplied_bgra(mut pixels: Vec<u8>) -> Vec<u8> {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = pixel[3] as u16;
        let scale = |channel: u8| ((channel as u16 * alpha + 127) / 255) as u8;
        let (red, green, blue) = (scale(pixel[0]), scale(pixel[1]), scale(pixel[2]));
        pixel[0] = blue;
        pixel[1] = green;
        pixel[2] = red;
    }
    pixels
}

/// 描く大きさ（幅, 高さ）。元の画素数か`|幅`を、表示倍率（百分率）ぶん。0にはしない。
pub fn size(picture: &Picture, width: Option<u32>, zoom_percent: i32) -> (u32, u32) {
    let (natural_width, natural_height) =
        (picture.width.max(1) as f64, picture.height.max(1) as f64);
    let wanted = width.map_or(natural_width, f64::from);
    let scale = wanted / natural_width * zoom_percent.max(1) as f64 / 100.0;
    (
        ((natural_width * scale).round() as u32).max(1),
        ((natural_height * scale).round() as u32).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_target_is_read_from_the_document_folder() {
        let folder = Path::new(r"D:\原稿");
        assert_eq!(
            resolve(Some(folder), "img/Pasted%20image.png"),
            Some(folder.join("img/Pasted image.png"))
        );
        assert_eq!(resolve(None, "img/a.png"), None);
        assert_eq!(
            resolve(None, r"C:\pictures\a.png"),
            Some(PathBuf::from(r"C:\pictures\a.png"))
        );
        // 戻せない並びは字のまま。
        assert_eq!(percent_decoded("100%.png"), "100%.png");
    }

    #[test]
    fn the_size_follows_the_width_option_and_the_zoom() {
        let picture = Picture {
            width: 400,
            height: 200,
            bgra: Vec::new(),
        };
        assert_eq!(size(&picture, None, 100), (400, 200));
        assert_eq!(size(&picture, Some(300), 100), (300, 150));
        assert_eq!(size(&picture, None, 150), (600, 300));
    }

    #[test]
    fn pixels_are_premultiplied_and_turned_to_bgra() {
        assert_eq!(
            premultiplied_bgra(vec![255, 128, 0, 255, 255, 255, 255, 0, 200, 100, 50, 128]),
            vec![0, 128, 255, 255, 0, 0, 0, 0, 25, 50, 100, 128]
        );
    }
}
