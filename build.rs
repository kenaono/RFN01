use std::path::PathBuf;
use std::process::Command;

/// Compile the Slint UI on a thread with room to recurse.
///
/// **Not on the build script's own thread.** A build script gets Windows'
/// default 1 MB stack, and the Slint compiler walks the syntax tree by
/// recursion — `ui/editor-pane.slint` grew past that while 要件 11.4's keys
/// were being added, and cargo reported only `STATUS_STACK_OVERFLOW` from the
/// build script, naming neither the file nor the line. Nothing about the UI was
/// wrong; there was simply no room to read it.
fn main() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            // 追加要件 2026-09-15（書き手）: 表示の国際化。`@tr("English")`の訳を
            // `translations/<言語>/LC_MESSAGES/rfnedit.po`から実行ファイルへ埋め込む。
            // **文脈は使わない**——同じ英語は同じ訳でよく、訳ファイルを部品の名前で割らない。
            let config = slint_build::CompilerConfiguration::new()
                .with_bundled_translations("translations")
                .with_default_translation_context(slint_build::DefaultTranslationContext::None);
            println!("cargo:rerun-if-changed=translations");
            slint_build::compile_with_config("ui/app-window.slint", config)
                .expect("failed to compile Slint UI")
        })
        .expect("failed to start the Slint compiler thread")
        .join()
        .expect("the Slint compiler thread panicked");

    embed_icon();
}

/// 実行ファイルのアイコンをWindowsのリソースとして埋め込む（追加要件 C3、2026-09-24）。
///
/// **新しいクレートを増やさない。**`embed-resource`などがしていることを、既にある
/// Windows SDKの`rc.exe`と、rustcがlink.exeへ渡す引数だけで行う。`.rc`はこの
/// ビルドの中で書き、リポジトリへは置かない——機械ごとに違う絶対パスを抱え込まないため。
///
/// **無い機械ではアイコン無しで通す。**SDKの場所は機械によって違うので、見つからなければ
/// 警告を出して先へ進む。ビルドが止まるより、アイコンが付かない方が害が小さい。
fn embed_icon() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icon = manifest.join("ui").join("icon.ico");
    println!("cargo:rerun-if-changed=ui/icon.ico");
    if !icon.is_file() {
        println!("cargo:warning=ui/icon.ico was not found; building without the file icon");
        return;
    }
    let Some(rc) = find_resource_compiler() else {
        println!("cargo:warning=rc.exe was not found; building without the file icon");
        return;
    };

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let script = out.join("rfnedit-icon.rc");
    let resource = out.join("rfnedit-icon.res");
    // **区切りは`/`で書く。**rc.exeは`\`を文字列の逃げ字として読むので、`\1`や`\5`が
    // 消えて`D:\Projects_Creation(_Dev_Editor\ui\icon.ico`のような存在しない道になり、
    // RC2135で止まる。rc.exeは`/`の道をそのまま開ける。
    let path = icon.to_string_lossy().replace('\\', "/");
    std::fs::write(&script, format!("1 ICON \"{path}\"\n"))
        .expect("failed to write the icon script");

    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg(&script)
        .status()
        .expect("failed to run rc.exe");
    assert!(status.success(), "rc.exe failed on {}", script.display());

    // link.exeは`.res`をそのまま受け取る。link.exeが組み立て直すので、
    // マニフェストなど他のリソースには触らない。
    println!("cargo:rustc-link-arg={}", resource.display());
}

/// 使う`rc.exe`を探す。PATHを先に見て、無ければWindows SDKを見る。
///
///  SDKは`Windows Kits/10/bin/<版>/<構成>/rc.exe`に置かれるので、
/// 版がいちばん新しいものを選ぶ。`rc.exe`はホストと同じ構成のものを使う。
fn find_resource_compiler() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let arch = if std::env::var("HOST")
        .unwrap_or_default()
        .starts_with("aarch64")
    {
        "arm64"
    } else {
        "x64"
    };
    let root = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Program Files (x86)"));
    let bin = root.join("Windows Kits").join("10").join("bin");
    let direct = bin.join(arch).join("rc.exe");
    if direct.is_file() {
        return Some(direct);
    }

    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for entry in std::fs::read_dir(bin).ok()?.flatten() {
        let candidate = entry.path().join(arch).join("rc.exe");
        if !candidate.is_file() {
            continue;
        }
        let version: Vec<u64> = entry
            .file_name()
            .to_string_lossy()
            .split('.')
            .map_while(|part| part.parse().ok())
            .collect();
        if version.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(best, _)| *best < version) {
            best = Some((version, candidate));
        }
    }
    best.map(|(_, path)| path)
}
