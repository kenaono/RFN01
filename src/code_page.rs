//! Windowsの文字コード表を、必要なぶんだけ（要件 E2）。
//!
//! **表を持たない。**CP932はWindowsの表であって、この編集器の表ではない
//! ——別の実装を持ち込めば、同じ原稿が道具によって違う字になる。ここにあるのは
//! `MultiByteToWideChar`と`WideCharToMultiByte`の薄い包みだけで、
//! 判断（何で読むか、読めない字をどうするか）は`file_io`の側にある。

use windows::Win32::Globalization::{
    MB_ERR_INVALID_CHARS, MultiByteToWideChar, WC_NO_BEST_FIT_CHARS, WideCharToMultiByte,
};

/// Windowsの日本語（Shift_JISの拡張、要件 E2）。
pub const CP932: u32 = 932;

/// そのバイト列を、その文字コードとして読む。
///
/// **1バイトでも表に無ければ`None`。**読めない字を`?`に替えて開くのは、壊れた原稿を
/// 確定することである（要件 E2：「文字化けした内容を確定しない」）——`MB_ERR_INVALID_CHARS`
/// がその「読めない」をWindowsに言わせている。
pub fn decode(code_page: u32, bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    // SAFETY: 長さはスライスから取っており、出力を渡さない呼びは「何文字になるか」
    // だけを答える（Windowsの決めごと）。
    let units = unsafe { MultiByteToWideChar(code_page, MB_ERR_INVALID_CHARS, bytes, None) };
    if units <= 0 {
        return None;
    }
    let mut wide = vec![0u16; units as usize];
    // SAFETY: buffer は上で数えたぶんだけ確保してある。
    let written =
        unsafe { MultiByteToWideChar(code_page, MB_ERR_INVALID_CHARS, bytes, Some(&mut wide)) };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    String::from_utf16(&wide).ok()
}

/// その文字コードで書く。
///
/// **表せない字があれば、その字を返す**（`Err`）。`?`に替えて黙って保存するのは、
/// 書き手の原稿を編集器が書き換えることである（要件 E2）——`WC_NO_BEST_FIT_CHARS`は
/// 「似た字で代用する」ことまで断らせる指定で、`—`が`-`になるような置き換えも起きない。
pub fn encode(code_page: u32, text: &str) -> Result<Vec<u8>, Vec<char>> {
    let missing = unrepresentable(code_page, text);
    if !missing.is_empty() {
        return Err(missing);
    }
    Ok(convert(code_page, text))
}

/// その文字コードで表せない字（重複なし、原稿に出てくる順）。
///
/// **1文字ずつ訊く。**Windowsは「代用したかどうか」を1回の変換につき1つの旗でしか
/// 答えないので、どの字かを言うにはそれしかない。表に無い字は原稿の中では
/// 珍しいので、費用は問題にならない——**普通に保存できる原稿では、この道は
/// 1文字も落とさずに終わる。**
fn unrepresentable(code_page: u32, text: &str) -> Vec<char> {
    let mut missing: Vec<char> = Vec::new();
    let mut buffer = [0u16; 2];
    for character in text.chars() {
        if character == '\n' || character.is_ascii() {
            continue;
        }
        let wide = character.encode_utf16(&mut buffer);
        let mut used = windows::core::BOOL::default();
        // SAFETY: 出力を渡さない呼びは長さだけを答える。旗は呼びのあいだ生きている。
        let length = unsafe {
            WideCharToMultiByte(
                code_page,
                WC_NO_BEST_FIT_CHARS,
                wide,
                None,
                windows::core::PCSTR::null(),
                Some(&mut used),
            )
        };
        if length <= 0 || used.as_bool() {
            if !missing.contains(&character) {
                missing.push(character);
            }
        }
    }
    missing
}

/// 表に有ることが分かっている字を、そのまま変換する。
fn convert(code_page: u32, text: &str) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: 出力を渡さない呼びは長さだけを答える。
    let length = unsafe {
        WideCharToMultiByte(
            code_page,
            WC_NO_BEST_FIT_CHARS,
            &wide,
            None,
            windows::core::PCSTR::null(),
            None,
        )
    };
    if length <= 0 {
        return Vec::new();
    }
    let mut bytes = vec![0u8; length as usize];
    // SAFETY: buffer は上で数えたぶんだけ確保してある。
    let written = unsafe {
        WideCharToMultiByte(
            code_page,
            WC_NO_BEST_FIT_CHARS,
            &wide,
            Some(&mut bytes),
            windows::core::PCSTR::null(),
            None,
        )
    };
    bytes.truncate(written.max(0) as usize);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E2: 日本語の原稿がCP932で往復する。
    #[test]
    fn japanese_text_goes_through_cp932_and_back() {
        let text = "春の海　ひねもすのたり";

        let bytes = encode(CP932, text).expect("CP932にある字");

        assert!(bytes.len() < text.len() * 2, "1文字2バイトで収まる");
        assert_eq!(decode(CP932, &bytes).as_deref(), Some(text));
    }

    /// E2: **表に無い字は、その字が返る**——`?`に替えない。
    #[test]
    fn a_character_the_table_lacks_comes_back_as_itself() {
        let missing = encode(CP932, "絵文字は🐈です").expect_err("表に無い");

        assert_eq!(missing, vec!['🐈']);
    }

    /// E2: **似た字での代用を断る。**ここが`WC_NO_BEST_FIT_CHARS`の効くところで、
    /// **日本語の原稿では絵文字より先にこれに当たる**：小説でよく使う`—`（U+2014
    /// EM DASH）はCP932の表に無く、Windowsは黙って`―`（U+2015 HORIZONTAL BAR、
    /// CP932の0x815C）へ寄せる。**寄せた時点で、原稿は書き手の書いた字ではない。**
    #[test]
    fn a_lookalike_is_not_a_substitute() {
        // U+2014は表に無い——代用させずに、その字を返す。
        assert_eq!(encode(CP932, "——").expect_err("表に無い"), vec!['—']);
        // U+2015は表にある（0x815C）ので、そのまま通る。
        assert_eq!(
            encode(CP932, "――").expect("表にある"),
            vec![0x81, 0x5C, 0x81, 0x5C]
        );
        // 拡張漢字も代用しない（`叱`にならない）。
        assert!(encode(CP932, "𠮟る").is_err());
    }

    /// E2: **CP932として読めないバイト列は`None`。**文字化けを確定しない。
    #[test]
    fn bytes_that_are_not_in_the_table_do_not_decode() {
        // UTF-8の「あ」はCP932では読めない並びである。
        assert_eq!(decode(CP932, "あ".as_bytes()), None);
    }
}
