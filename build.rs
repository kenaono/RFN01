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
}
