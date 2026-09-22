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

/// 行き先をファイルへ。絶対パスはそのまま、相対パスは文書のフォルダから。**名前の無い文書**
/// （フォルダが無い）では相対パスを解決しない。
pub fn resolve(folder: Option<&Path>, target: &str) -> Option<PathBuf> {
    // **戻し方はリンクと同じ**（`%20`などを1回だけ戻す）。
    let decoded = crate::link_completion::percent_decode(target);
    let path = Path::new(&decoded);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let direct = folder.map(|f| f.join(path));
    if direct.as_ref().is_some_and(|p| p.is_file()) {
        return direct;
    }
    INDEX
        .with(|index| {
            if !index.borrow().2 {
                return None;
            }
            let source = folder.map(|f| f.join("__source__.md"));
            crate::workspace_links::resolve_indexed_file(
                &decoded,
                true,
                source.as_deref(),
                &index.borrow().1,
                true,
            )
            .ok()
        })
        .or(direct)
}

thread_local! {
    static INDEX: RefCell<(u64, std::sync::Arc<Vec<crate::workspace_index::Entry>>, bool)> = RefCell::new((0, std::sync::Arc::new(Vec::new()), false));
}
/// Publishes the shared view (a clone-free [`std::sync::Arc`] of the index
/// the caller already holds) for whoever resolves an image by name. Returns
/// whether the picture index actually changed, so a caller can skip a repaint
/// that would redraw identical pictures.
pub fn publish_index(
    entries: std::sync::Arc<Vec<crate::workspace_index::Entry>>,
    complete: bool,
) -> bool {
    INDEX.with(|index| {
        let mut current = index.borrow_mut();
        if *current.1 == *entries && current.2 == complete {
            return false;
        }
        current.0 = current.0.wrapping_add(1);
        current.1 = entries;
        current.2 = complete;
        true
    })
}
pub fn index_revision() -> u64 {
    INDEX.with(|i| i.borrow().0)
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

/// 引いて変えられる、いちばん小さい幅（画面の画素）。
const SMALLEST: f32 = 16.0;

/// 絵の角を引いた先から決まる大きさ（追加要件 2026-09-16：絵の大きさをマウスで変える）。
///
/// `rect`は今の絵（面の座標で左・上・幅・高さ）、`to`は引いている点。**止まっている角は、組み直しても
/// 絵が動かない角**——横書きは左上、縦書きは右上（縦書きは右から積むので、列が太っても右端は動かない）。
/// 大きさは引いた点を対角線へ写して決め、縦横比を保つ。行に沿う長さは`line_box`まで（組むときも
/// そこで縮む）、幅は`SMALLEST`から。
///
/// 返すのは、記法に書く幅（表示倍率を外した画素）と、引いているあいだの枠（面の座標）。
pub fn resized(
    rect: [f32; 4],
    vertical: bool,
    to: (f32, f32),
    line_box: f32,
    zoom_percent: i32,
) -> (u32, [f32; 4]) {
    let [left, top, width, height] = rect;
    let (width, height) = (width.max(1.0), height.max(1.0));
    let across = if vertical {
        left + width - to.0
    } else {
        to.0 - left
    };
    let down = to.1 - top;
    let along = if vertical { height } else { width };
    let scale = ((across * width + down * height) / (width * width + height * height))
        .min(line_box.max(1.0) / along)
        .max(SMALLEST / width);
    let (wide, tall) = (width * scale, height * scale);
    let x = if vertical { left + width - wide } else { left };
    let zoom = zoom_percent.max(1) as f32 / 100.0;
    ((wide / zoom).round().max(1.0) as u32, [x, top, wide, tall])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_index_publication_resolves_names_and_scope_switch_invalidates_them() {
        let root = std::env::temp_dir().join(format!(
            "rfn-image-index-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::create_dir_all(root.join("notes")).unwrap();
        let mut bmp = b"BM".to_vec();
        bmp.extend(70u32.to_le_bytes());
        bmp.extend([0; 4]);
        bmp.extend(54u32.to_le_bytes());
        bmp.extend(40u32.to_le_bytes());
        bmp.extend(2i32.to_le_bytes());
        bmp.extend(2i32.to_le_bytes());
        bmp.extend(1u16.to_le_bytes());
        bmp.extend(24u16.to_le_bytes());
        bmp.extend([0; 24]);
        bmp.extend([0, 0, 255, 0, 255, 0, 0, 0, 255, 0, 0, 255, 255, 255, 0, 0]);
        let path = root.join("assets/picture.bmp");
        std::fs::write(&path, bmp).unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let entry =
            crate::workspace_index::read_entry(&path, std::slice::from_ref(&canonical_root))
                .unwrap();
        let folder = root.join("notes");
        publish_index(std::sync::Arc::new(vec![entry.clone()]), false);
        assert!(load(&resolve(Some(&folder), "picture.bmp").unwrap()).is_none());
        let before = index_revision();
        publish_index(std::sync::Arc::new(vec![entry.clone()]), true);
        assert_ne!(index_revision(), before);
        assert_eq!(
            resolve(Some(&folder), "picture.bmp"),
            Some(entry.canonical.clone())
        );
        let picture =
            load(&entry.canonical).expect("indexed BMP decodes using the normal renderer");
        assert_eq!((picture.width, picture.height), (2, 2));
        publish_index(std::sync::Arc::new(Vec::new()), false);
        assert!(load(&resolve(Some(&folder), "picture.bmp").unwrap()).is_none());
    }

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
        assert_eq!(
            resolve(Some(folder), "100%.png"),
            Some(folder.join("100%.png"))
        );
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
    fn dragging_a_corner_keeps_the_shape_and_the_fixed_corner() {
        // 横書き：左上が止まる。右下の角を右下へ50%引けば1.5倍。
        let (width, outline) = resized(
            [10.0, 20.0, 200.0, 100.0],
            false,
            (310.0, 170.0),
            600.0,
            100,
        );
        assert_eq!((width, outline), (300, [10.0, 20.0, 300.0, 150.0]));
        // 縦書き：右上が止まる。左下の角を左へ引く。表示倍率200%なら書く幅は半分。
        let (width, outline) = resized([100.0, 20.0, 200.0, 100.0], true, (0.0, 170.0), 600.0, 200);
        assert_eq!((width, outline), (150, [0.0, 20.0, 300.0, 150.0]));
        // 行に沿う長さは行まで、幅は16pxから。
        assert_eq!(
            resized(
                [10.0, 20.0, 200.0, 100.0],
                false,
                (900.0, 900.0),
                400.0,
                100
            )
            .0,
            400
        );
        assert_eq!(
            resized([10.0, 20.0, 200.0, 100.0], false, (0.0, 0.0), 400.0, 100).0,
            16
        );
        assert_eq!(
            resized(
                [100.0, 20.0, 200.0, 100.0],
                true,
                (-500.0, 900.0),
                150.0,
                100
            )
            .0,
            300
        );
    }

    #[test]
    fn pixels_are_premultiplied_and_turned_to_bgra() {
        assert_eq!(
            premultiplied_bgra(vec![255, 128, 0, 255, 255, 255, 255, 0, 200, 100, 50, 128]),
            vec![0, 128, 255, 255, 0, 0, 0, 0, 25, 50, 100, 128]
        );
    }
}
