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
        .spawn(|| slint_build::compile("ui/app-window.slint").expect("failed to compile Slint UI"))
        .expect("failed to start the Slint compiler thread")
        .join()
        .expect("the Slint compiler thread panicked");
}
