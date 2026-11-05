//! Shell profiles: editable launch settings; existing processes are unchanged.
use super::*;

pub(crate) fn command_parts(command: &str) -> (&str, &str) {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (&rest[..end], rest[end + 1..].trim());
        }
    }
    command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(a, b)| (a, b.trim()))
}

/// RFN01-95: the Windows folder a shell started in, for reading the file
/// names it prints. **Empty is the editor's own folder** (the shell inherits
/// it); WSL's `/mnt/d/…` is `D:\…`; any other Linux folder has no Windows name
/// this side can use.
pub(crate) fn windows_folder(directory: &str) -> Option<PathBuf> {
    let directory = directory.trim();
    if directory.is_empty() {
        return std::env::current_dir().ok();
    }
    if let Some(path) = wsl_mount(directory) {
        return Some(path);
    }
    if directory.starts_with('/') || directory.starts_with('~') {
        return None;
    }
    Some(PathBuf::from(directory))
}

/// `/mnt/d/x/y` as `D:\x\y`. Anything else is not a mounted Windows drive.
pub(crate) fn wsl_mount(path: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix("/mnt/")?;
    let mut chars = rest.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    let after = chars.as_str();
    if !(after.is_empty() || after.starts_with('/')) {
        return None;
    }
    let tail = after.trim_start_matches('/').replace('/', "\\");
    Some(PathBuf::from(format!(
        "{}:\\{tail}",
        drive.to_ascii_uppercase()
    )))
}

/// RFN01-100: this shell is WSL, and which distribution it runs (`-d`,
/// `--distribution`); `Some(None)` is the default one.
pub(crate) fn wsl_distribution(command: &str) -> Option<Option<String>> {
    let (program, args) = command_parts(command);
    let wsl = Path::new(program)
        .file_stem()
        .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case("wsl"));
    if !wsl {
        return None;
    }
    let mut words = args.split_whitespace();
    while let Some(word) = words.next() {
        if word == "-d" || word == "--distribution" {
            return Some(words.next().map(|name| name.trim_matches('"').to_owned()));
        }
        if word == "--" || word == "-e" || word == "--exec" {
            break;
        }
    }
    Some(None)
}

/// RFN01-100: what WSL said a Linux path is, or that it is still being asked.
#[derive(Clone, Debug, PartialEq)]
enum Lookup {
    Asking,
    Known(Option<PathBuf>),
}

type Lookups = std::sync::Mutex<std::collections::HashMap<(Option<String>, String), Lookup>>;

fn lookups() -> &'static Lookups {
    static LOOKUPS: std::sync::OnceLock<Lookups> = std::sync::OnceLock::new();
    LOOKUPS.get_or_init(Default::default)
}

/// RFN01-100（書き手と合意 2026-11-04）: a Linux path in WSL — `~/Dev/x`,
/// `/home/ken/novel` — as the Windows file it is, **following symbolic
/// links**. Only a file on a Windows drive (under `/mnt`) has an answer: one
/// that is really inside WSL (`\\wsl.localhost\…`) is not opened (書き手：
/// 「あくまで、Windowsに共有されているmnt配下のみ」).
///
/// Each path is asked once and remembered. With `wait` false the first ask
/// happens on a thread of its own, and `None` comes back at once; `answered`
/// runs when the answer is in, so the pointer's question can be asked again.
pub(crate) fn wsl_windows_path(
    distribution: Option<&str>,
    linux: &str,
    wait: bool,
    answered: impl FnOnce() + Send + 'static,
) -> Option<PathBuf> {
    let key = (distribution.map(str::to_owned), linux.to_owned());
    let known = lookups().lock().ok()?.get(&key).cloned();
    match known {
        Some(Lookup::Known(answer)) => return answer,
        Some(Lookup::Asking) if !wait => return None,
        _ => {}
    }
    if wait {
        let answer = ask_wsl(key.0.as_deref(), &key.1);
        if let Ok(mut table) = lookups().lock() {
            table.insert(key, Lookup::Known(answer.clone()));
        }
        return answer;
    }
    if let Ok(mut table) = lookups().lock() {
        table.insert(key.clone(), Lookup::Asking);
    }
    let _ = std::thread::Builder::new()
        .name("rfnedit-wslpath".to_owned())
        .spawn(move || {
            let answer = ask_wsl(key.0.as_deref(), &key.1);
            if let Ok(mut table) = lookups().lock() {
                table.insert(key, Lookup::Known(answer));
            }
            answered();
        });
    None
}

/// Ask WSL once: `~` as the home, links followed (`realpath -m`), then the
/// Windows name (`wslpath -w`). About 0.08s while WSL is running.
fn ask_wsl(distribution: Option<&str>, linux: &str) -> Option<PathBuf> {
    let mut command = std::process::Command::new("wsl.exe");
    if let Some(name) = distribution {
        command.args(["-d", name]);
    }
    command
        .args([
            "-e",
            "sh",
            "-c",
            r#"p="$1"; case "$p" in "~") p="$HOME";; "~/"*) p="$HOME/${p#"~/"}";; esac; wslpath -w "$(realpath -m -- "$p")""#,
            "sh",
            linux,
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // コンソールの窓を一瞬でも出さない（git.rsと同じ）。
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    windows_drive_path(String::from_utf8_lossy(&output.stdout).trim())
}

/// `D:\…` only — not `\\wsl.localhost\…` nor anything else.
pub(crate) fn windows_drive_path(text: &str) -> Option<PathBuf> {
    let bytes = text.as_bytes();
    (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &text[1..3] == ":\\")
        .then(|| PathBuf::from(text))
}

/// Whether this profile starts WSL (`wsl.exe`).
pub(crate) fn is_wsl(shell: &TerminalShell) -> bool {
    let (program, _) = command_parts(&shell.command);
    Path::new(program)
        .file_stem()
        .is_some_and(|p| p.to_string_lossy().eq_ignore_ascii_case("wsl"))
}

/// RFN01-105（書き手と合意 2026-11-05）: **開始フォルダが空のプロファイルは、Workspaceの
/// フォルダかホームで始める。**以前は何も渡さず、エディタ自身の作業フォルダ（起動した場所）を
/// 継いでいた——新しいTerminalがホームにならず、Workspaceを開いても外のフォルダのままだった。
///
/// `workspace`は使用中のWorkspaceの最初の登録フォルダ、Workspaceを使っていなければExplorerで
/// 開いているフォルダ（呼ぶ側が決める）。無ければホーム：WSLは`~`、ほかは`%USERPROFILE%`。
/// 指定のあるプロファイルはそのまま。
pub(crate) fn with_start_folder(shell: &TerminalShell, workspace: Option<&Path>) -> TerminalShell {
    let mut started = shell.clone();
    if !started.directory.trim().is_empty() {
        return started;
    }
    started.directory = match workspace {
        Some(folder) => folder.to_string_lossy().into_owned(),
        None if is_wsl(shell) => "~".to_owned(),
        None => std::env::var("USERPROFILE").unwrap_or_default(),
    };
    started
}

pub(crate) fn launch(shell: &TerminalShell) -> (String, Option<PathBuf>) {
    let cwd = shell.directory.trim();
    let (program, args) = command_parts(&shell.command);
    let wsl = is_wsl(shell);
    // WSL inherits a Windows working directory through CreateProcess. Linux
    // paths are passed to WSL itself, never used as Windows currentDirectory.
    if wsl && (cwd.starts_with('/') || cwd == "~") {
        let path = cwd.replace('"', "\\\"");
        (format!("\"{program}\" --cd \"{path}\" {args}"), None)
    } else {
        (
            shell.command.clone(),
            (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
        )
    }
}

fn publish(window: &AppWindow, index: usize) {
    let shells = configured_shells(window);
    let default_name = shell_at(window, window.get_default_shell()).name;
    window.set_shell_profile_names(ModelRc::new(VecModel::from(
        shells
            .iter()
            .map(|s| {
                SharedString::from(if s.name == default_name {
                    format!("{} ({})", s.name, pick("既定", "Default"))
                } else {
                    s.name.clone()
                })
            })
            .collect::<Vec<_>>(),
    )));
    let index = index.min(shells.len().saturating_sub(1));
    window.set_shell_profile_index(index as i32);
    window.set_shell_profile_default(shells.get(index).is_some_and(|s| s.name == default_name));
    if let Some(shell) = shells.get(index) {
        let (exe, args) = command_parts(&shell.command);
        window.set_shell_profile_name(shell.name.clone().into());
        window.set_shell_profile_executable(exe.into());
        window.set_shell_profile_arguments(args.into());
        window.set_shell_profile_directory(shell.directory.clone().into());
    }
}

pub(crate) fn action(window: &AppWindow, live: &Live, what: i32, at: i32) {
    let mut shells = configured_shells(window);
    let mut index = window.get_shell_profile_index().max(0) as usize;
    index = index.min(shells.len().saturating_sub(1));
    let old_default = shell_at(window, window.get_default_shell()).name;
    let editing_default = shells.get(index).is_some_and(|s| s.name == old_default);
    // 書き手の報告 2026-09-24（引数を変えてもApplyできない）：押された操作と、どのプロファイルに
    // 効いたかを残す。手元の試験では再現しなかったので、実際の実行の記録で確かめる。
    live.cache.borrow_mut().log_diag(
        "spec.shell",
        &format!(
            "profile action={what} at={at} index={index} count={}",
            shells.len()
        ),
    );
    match what {
        0 => {
            publish(window, at.max(0) as usize);
            return;
        }
        1 => {
            let name = window.get_shell_profile_name().trim().to_owned();
            let exe = window.get_shell_profile_executable().trim().to_owned();
            let args = window.get_shell_profile_arguments().trim().to_owned();
            let directory = window.get_shell_profile_directory().trim().to_owned();
            live.cache.borrow_mut().log_diag(
                "spec.shell",
                &format!("profile apply name={name:?} exe={exe:?} args={args:?} dir={directory:?}"),
            );
            if name.is_empty()
                || exe.is_empty()
                || name.contains(['|', '\r', '\n', '\t'])
                || exe.contains(['"', '\r', '\n', '\t'])
                || args.contains(['\r', '\n', '\t'])
                || directory.contains(['\r', '\n', '\t', '"'])
                || shells
                    .iter()
                    .enumerate()
                    .any(|(i, s)| i != index && s.name == name)
            {
                live.cache.borrow_mut().log_diag(
                    "spec.shell",
                    &format!(
                        "profile refused name_empty={} exe_empty={} name_bad={} exe_bad={} args_bad={} dir_bad={}",
                        name.is_empty(),
                        exe.is_empty(),
                        name.contains(['|', '\r', '\n', '\t']),
                        exe.contains(['"', '\r', '\n', '\t']),
                        args.contains(['\r', '\n', '\t']),
                        directory.contains(['\r', '\n', '\t', '"']),
                    ),
                );
                window.tell(pick("名前と実行ファイルを確認してください。同じ名前や改行は使えません", "Check the name and executable; names must be unique and fields cannot contain line breaks").into());
                return;
            }
            shells[index] = TerminalShell {
                name,
                command: format!("\"{exe}\" {args}").trim().to_owned(),
                directory,
            };
        }
        2 => {
            shells.push(TerminalShell {
                name: format!("Shell {}", shells.len() + 1),
                command: "powershell.exe -NoLogo".into(),
                directory: String::new(),
            });
            index = shells.len() - 1;
        }
        3 => {
            let mut copy = shells[index].clone();
            copy.name = format!("{} ({})", copy.name, shells.len() + 1);
            shells.push(copy);
            index = shells.len() - 1;
        }
        4 if index > 0 => {
            shells.swap(index, index - 1);
            index -= 1;
        }
        5 if index + 1 < shells.len() => {
            shells.swap(index, index + 1);
            index += 1;
        }
        6 if shells.len() > 1 => {
            shells.remove(index);
            index = index.min(shells.len() - 1);
        }
        7 => {}
        8 => {
            let directory = live
                .active(window)
                .file
                .borrow()
                .path()
                .and_then(Path::parent)
                .map(Path::to_path_buf);
            if let Some(directory) = directory {
                open_in(window, live, directory);
            } else {
                window
                    .tell(pick("保存済みの文書を選んでください", "Choose a saved document").into());
            }
            return;
        }
        _ => return,
    }
    let wanted = if what == 7 || (what == 1 && editing_default) {
        shells[index].name.clone()
    } else {
        old_default
    };
    hold_shells(window, &shells);
    publish_shells(window);
    if let Some(index) = offered_shells(window).iter().position(|s| s.name == wanted) {
        window.set_default_shell(index as i32);
    }
    publish(window, index);
    save_settings(window, &live.cache);
    live.cache.borrow_mut().log_diag(
        "spec.shell",
        &format!("profile saved action={what} index={index}"),
    );
}

pub(crate) fn open_in(window: &AppWindow, live: &Live, directory: PathBuf) {
    let mut shell = shell_at(window, window.get_default_shell());
    shell.directory = directory.to_string_lossy().into_owned();
    open_terminal(window, live, focused_pane(window), shell);
}

pub(crate) fn install(window: &AppWindow, live: &Live) {
    publish(window, 0);
    let weak = window.as_weak();
    let live = live.clone();
    window.on_shell_profile_action(move |what, at| {
        if let Some(window) = weak.upgrade() {
            action(&window, &live, what, at);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFN01-100: which shells are WSL, and which distribution they run.
    #[test]
    fn wsl_and_its_distribution_are_read_from_the_command() {
        assert_eq!(wsl_distribution("wsl.exe"), Some(None));
        assert_eq!(
            wsl_distribution("wsl -d Ubuntu-24.04"),
            Some(Some("Ubuntu-24.04".to_owned()))
        );
        assert_eq!(
            wsl_distribution("wsl.exe --distribution Debian --cd ~"),
            Some(Some("Debian".to_owned()))
        );
        assert_eq!(wsl_distribution("wsl.exe -- bash -d x"), Some(None));
        assert_eq!(wsl_distribution("powershell.exe -NoLogo"), None);
        assert_eq!(wsl_distribution("cmd.exe"), None);
    }

    /// RFN01-100: only a Windows drive is an answer.
    #[test]
    fn only_a_windows_drive_is_opened() {
        assert_eq!(
            windows_drive_path("D:\\Projects\\原稿"),
            Some(PathBuf::from("D:\\Projects\\原稿"))
        );
        assert_eq!(
            windows_drive_path("\\\\wsl.localhost\\Ubuntu\\home\\ken"),
            None
        );
        assert_eq!(windows_drive_path(""), None);
        assert_eq!(windows_drive_path("/home/ken"), None);
    }

    /// RFN01-100: WSL itself follows a link onto a Windows drive, and a folder
    /// that is really inside WSL has no answer.
    #[test]
    #[ignore = "asks the real WSL; run explicitly"]
    fn wsl_follows_links_onto_windows_drives() {
        let made = std::process::Command::new("wsl.exe")
            .args(["-e", "ln", "-sfn", "/mnt/c/Windows", "/tmp/rfn-link-test"])
            .status()
            .unwrap();
        assert!(made.success());
        let answer = wsl_windows_path(None, "/tmp/rfn-link-test/System32", true, || {});
        assert_eq!(
            answer.map(|p| p.to_string_lossy().to_ascii_lowercase()),
            Some("c:\\windows\\system32".to_owned())
        );
        assert_eq!(
            wsl_windows_path(None, "~", true, || {}),
            None,
            "the home is WSL's own"
        );
        // Asked again, it is remembered (and answered without waiting).
        assert!(wsl_windows_path(None, "/tmp/rfn-link-test/System32", false, || {}).is_some());
        // Not asked yet and not waited for: nothing now, the answer arrives.
        let (told, heard) = std::sync::mpsc::channel();
        assert_eq!(
            wsl_windows_path(None, "/tmp/rfn-link-test", false, move || {
                let _ = told.send(());
            }),
            None
        );
        heard
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(wsl_windows_path(None, "/tmp/rfn-link-test", false, || {}).is_some());
    }

    #[test]
    fn quoted_program_and_japanese_directory_roundtrip() {
        let shell = TerminalShell {
            name: "My Shell".into(),
            command: "\"C:\\Program Files\\PowerShell\\pwsh.exe\" -NoLogo".into(),
            directory: "D:\\原稿 作業".into(),
        };
        assert_eq!(shell.program(), "C:\\Program Files\\PowerShell\\pwsh.exe");
        assert_eq!(TerminalShell::read(&shell.written()), Some(shell.clone()));
        assert_eq!(
            launch(&shell),
            (shell.command.clone(), Some(PathBuf::from(&shell.directory)))
        );
        let linux = TerminalShell {
            name: "WSL".into(),
            command: "wsl.exe -d Ubuntu".into(),
            directory: "/home/user/原稿 作業".into(),
        };
        assert_eq!(
            launch(&linux),
            (
                "\"wsl.exe\" --cd \"/home/user/原稿 作業\" -d Ubuntu".into(),
                None
            )
        );
    }

    /// RFN01-105: 開始フォルダが空なら Workspace のフォルダ、無ければホーム（WSLは`~`）。
    /// 指定のあるプロファイルはそのまま。
    #[test]
    fn an_empty_start_folder_opens_in_the_workspace_or_home() {
        let windows = TerminalShell {
            name: "PowerShell".into(),
            command: "powershell.exe -NoLogo".into(),
            directory: String::new(),
        };
        let wsl = TerminalShell {
            name: "WSL".into(),
            command: "wsl.exe -d Ubuntu".into(),
            directory: "  ".into(),
        };
        let workspace = Path::new("D:\\原稿");
        assert_eq!(
            with_start_folder(&windows, Some(workspace)).directory,
            "D:\\原稿"
        );
        assert_eq!(
            with_start_folder(&wsl, Some(workspace)).directory,
            "D:\\原稿"
        );
        assert_eq!(
            with_start_folder(&windows, None).directory,
            std::env::var("USERPROFILE").unwrap_or_default()
        );
        assert_eq!(with_start_folder(&wsl, None).directory, "~");
        assert_eq!(
            launch(&with_start_folder(&wsl, None)).0,
            "\"wsl.exe\" --cd \"~\" -d Ubuntu"
        );
        // A Windows folder reaches WSL as the working folder it inherits.
        assert_eq!(
            launch(&with_start_folder(&wsl, Some(workspace))).1,
            Some(PathBuf::from("D:\\原稿"))
        );
        let named = TerminalShell {
            directory: "C:\\work".into(),
            ..windows.clone()
        };
        assert_eq!(with_start_folder(&named, Some(workspace)), named);
    }
}
