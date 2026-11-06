//! Shell profiles: editable launch settings; existing processes are unchanged.
use super::*;
use std::collections::{HashMap, HashSet};

/// `--quiet` emits one registered name per line. Some WSL versions use
/// UTF-16LE when stdout is redirected, even without a BOM.
fn distribution_names(bytes: &[u8]) -> Vec<String> {
    let decoded = if bytes.starts_with(&[0xff, 0xfe]) || bytes.contains(&0) {
        let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
        String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    decoded
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|name| {
            !name.is_empty() && !name.contains(['"', '\0']) && !name.contains(char::is_whitespace)
        })
        .map(str::to_owned)
        .collect()
}

fn installed_distributions() -> Vec<String> {
    if !on_path("wsl.exe") {
        return Vec::new();
    }
    let mut command = std::process::Command::new("wsl.exe");
    command.args(["--list", "--quiet"]);
    wsl_output(command).map_or_else(Vec::new, |output| distribution_names(&output))
}

pub(crate) fn discover_distributions(window: &AppWindow) {
    if window.get_wsl_discovery_running() {
        return;
    }
    window.set_wsl_discovery_running(true);
    let weak = window.as_weak();
    std::thread::spawn(move || {
        let names = installed_distributions();
        let _ = weak.upgrade_in_event_loop(move |window| {
            window.set_wsl_distributions(ModelRc::new(VecModel::from(
                names
                    .into_iter()
                    .map(SharedString::from)
                    .collect::<Vec<_>>(),
            )));
            window.set_wsl_discovery_complete(true);
            window.set_wsl_discovery_running(false);
            publish_shells(&window);
        });
    });
}

pub(crate) fn unavailable_reason(
    window: &AppWindow,
    shell: &TerminalShell,
) -> Option<&'static str> {
    if !on_path(shell.program()) {
        return Some(pick("実行ファイルが見つかりません", "Executable not found"));
    }
    if window.get_wsl_discovery_complete()
        && let Some(distribution) = wsl_distribution(&shell.command)
    {
        let installed = window.get_wsl_distributions();
        let available = match distribution {
            Some(name) => installed
                .iter()
                .any(|installed| installed.eq_ignore_ascii_case(&name)),
            None => installed.row_count() > 0,
        };
        if !available {
            return Some(pick(
                "WSL ディストリビューションを利用できません",
                "WSL distribution unavailable",
            ));
        }
    }
    None
}

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
    Some(wsl_distribution_value(args).map(|value| command_parts(value).0.to_owned()))
}

fn wsl_distribution_value(args: &str) -> Option<&str> {
    let mut remaining = args;
    while !remaining.is_empty() {
        let (word, rest) = command_parts(remaining);
        if word == "-d" || word == "--distribution" {
            return (!rest.is_empty()).then_some(rest);
        }
        if !word.starts_with('-') || matches!(word, "--" | "-e" | "--exec") {
            break;
        }
        remaining = if matches!(word, "--cd" | "-u" | "--user" | "--shell-type") {
            command_parts(rest).1
        } else {
            rest
        };
    }
    None
}

const WSL_LOOKUP_WORKERS: usize = 4;
const WSL_LOOKUP_ENTRIES: usize = 128;
type LookupKey = (Option<String>, String);

#[derive(Default)]
struct Lookups {
    known: HashMap<LookupKey, (Instant, Option<PathBuf>)>,
    hovering: HashSet<LookupKey>,
    workers: usize,
}

impl Lookups {
    fn remember(&mut self, key: LookupKey, answer: Option<PathBuf>, now: Instant) {
        self.known.retain(|_, (expires, _)| *expires > now);
        if self.known.len() >= WSL_LOOKUP_ENTRIES
            && let Some(oldest) = self
                .known
                .iter()
                .min_by_key(|(_, (expires, _))| *expires)
                .map(|(key, _)| key.clone())
        {
            self.known.remove(&oldest);
        }
        let ttl = if answer.is_some() {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(250)
        };
        self.known.insert(key, (now + ttl, answer));
    }
}

fn lookups() -> &'static std::sync::Mutex<Lookups> {
    static LOOKUPS: std::sync::OnceLock<std::sync::Mutex<Lookups>> = std::sync::OnceLock::new();
    LOOKUPS.get_or_init(Default::default)
}

struct LookupSlot(Option<LookupKey>);

impl Drop for LookupSlot {
    fn drop(&mut self) {
        let mut state = lookups().lock().unwrap();
        state.workers -= 1;
        if let Some(key) = &self.0 {
            state.hovering.remove(key);
        }
    }
}

/// Hover can use a recent conversion. Leave one worker slot for clicks.
pub(crate) fn cached_wsl_windows_path(
    distribution: Option<&str>,
    linux: &str,
    answered: impl FnOnce() + Send + 'static,
) -> Option<PathBuf> {
    let key = (distribution.map(str::to_ascii_lowercase), linux.to_owned());
    let mut state = lookups().lock().unwrap();
    if let Some((expires, answer)) = state.known.get(&key)
        && *expires > Instant::now()
    {
        return answer.clone();
    }
    if state.workers >= WSL_LOOKUP_WORKERS - 1 || !state.hovering.insert(key.clone()) {
        return None;
    }
    state.workers += 1;
    drop(state);
    let slot = LookupSlot(Some(key.clone()));
    let distribution = distribution.map(str::to_owned);
    let _ = std::thread::Builder::new()
        .name("rfnedit-wslpath".to_owned())
        .spawn(move || {
            let answer = ask_wsl(distribution.as_deref(), &key.1);
            lookups()
                .lock()
                .unwrap()
                .remember(key, answer, Instant::now());
            drop(slot);
            answered();
        });
    None
}

pub(crate) fn start_fresh_wsl_lookup(resolve: impl FnOnce() + Send + 'static) -> bool {
    let mut state = lookups().lock().unwrap();
    if state.workers >= WSL_LOOKUP_WORKERS {
        return false;
    }
    state.workers += 1;
    drop(state);
    let slot = LookupSlot(None);
    std::thread::Builder::new()
        .name("rfnedit-wslopen".to_owned())
        .spawn(move || {
            resolve();
            drop(slot);
        })
        .is_ok()
}

/// Ask WSL once: `~` as the home, links followed (`realpath -m`), then the
/// Windows name (`wslpath -w`). About 0.08s while WSL is running.
pub(crate) fn ask_wsl(distribution: Option<&str>, linux: &str) -> Option<PathBuf> {
    let mut command = std::process::Command::new("wsl.exe");
    if let Some(name) = distribution {
        command.args(["-d", name]);
    }
    command.args([
        "-e",
        "sh",
        "-c",
        r#"p="$1"; case "$p" in "~") p="$HOME";; "~/"*) p="$HOME/${p#"~/"}";; esac; wslpath -w "$(realpath -m -- "$p")""#,
        "sh",
        linux,
    ]);
    let output = wsl_output(command)?;
    windows_drive_path(String::from_utf8_lossy(&output).trim())
}

fn wsl_output(mut command: std::process::Command) -> Option<Vec<u8>> {
    command
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // コンソールの窓を一瞬でも出さない（git.rsと同じ）。
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    output.status.success().then_some(output.stdout)
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
        None => std::env::var("USERPROFILE")
            .ok()
            .filter(|profile| !profile.trim().is_empty())
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|dir| dir.to_string_lossy().into_owned())
            })
            .unwrap_or_default(),
    };
    started
}

pub(crate) fn launch(shell: &TerminalShell) -> (String, Option<PathBuf>) {
    let cwd = shell.directory.trim();
    let (program, args) = command_parts(&shell.command);
    let wsl = is_wsl(shell);
    // WSL parses this option itself and keeps the quotes as part of the name.
    // Repair profiles saved by the first RFN01-132 build at launch time.
    let corrected = wsl
        .then(|| wsl_distribution_value(args))
        .flatten()
        .and_then(|value| {
            let (name, tail) = value.strip_prefix('"')?.split_once('"')?;
            if name.is_empty()
                || name.starts_with('-')
                || name.contains(char::is_whitespace)
                || (!tail.is_empty() && !tail.starts_with(char::is_whitespace))
            {
                return None;
            }
            let prefix = &args[..args.len() - value.len()];
            Some(format!("{prefix}{name}{tail}"))
        });
    let args = corrected.as_deref().unwrap_or(args);
    // WSL inherits a Windows working directory through CreateProcess. Linux
    // paths are passed to WSL itself, never used as Windows currentDirectory.
    if wsl && (cwd.starts_with('/') || cwd == "~") {
        let path = cwd.replace('"', "\\\"");
        (format!("\"{program}\" --cd \"{path}\" {args}"), None)
    } else {
        (
            if corrected.is_some() {
                format!("\"{program}\" {args}")
            } else {
                powershell_launch(program, args).unwrap_or_else(|| shell.command.clone())
            },
            (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
        )
    }
}

fn powershell_launch(program: &str, args: &str) -> Option<String> {
    let stem = Path::new(program).file_stem()?.to_str()?;
    if !["powershell", "pwsh"]
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
        || !args.split_whitespace().all(|arg| {
            ["-NoLogo", "-NoProfile"]
                .iter()
                .any(|flag| arg.eq_ignore_ascii_case(flag))
        })
    {
        return None;
    }
    // Wrap the host's existing reader so profiles and PSReadLine key bindings
    // still take effect. If the reader is unavailable the session stays unknown.
    let hook = concat!(
        "if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage' ",
        "-and (Get-Command PSConsoleHostReadLine -CommandType Function -ErrorAction Ignore)) { ",
        "$global:RFNEditOriginalReadLine = ${function:PSConsoleHostReadLine}; ",
        "function global:PSConsoleHostReadLine { ",
        "[Console]::Write(('{0}]133;A{1}' -f [char]27,[char]7)); ",
        "$line = & $global:RFNEditOriginalReadLine; ",
        "[Console]::Write(('{0}]133;C{1}' -f [char]27,[char]7)); ",
        "$line } }"
    );
    Some(format!("\"{program}\" {args} -NoExit -Command \"{hook}\""))
}

/// Supply Bash's prompt hook through the WSL child's environment. WSL still
/// chooses the login shell and runs its regular startup files. A startup file
/// that sets its own PROMPT_COMMAND takes precedence.
pub(crate) fn wsl_shell_environment(shell: &TerminalShell) -> Option<Vec<u16>> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;

    if !is_wsl(shell) {
        return None;
    }
    let (_, mut args) = command_parts(&shell.command);
    while !args.is_empty() {
        let (option, rest) = command_parts(args);
        let (value, rest) = command_parts(rest);
        if !matches!(option, "-d" | "--distribution" | "-u" | "--user")
            || value.is_empty()
            || value.starts_with('-')
        {
            return None;
        }
        args = rest;
    }
    let mut variables: Vec<_> = std::env::vars_os().collect();
    let named = |key: &OsString, name: &str| key.to_string_lossy().eq_ignore_ascii_case(name);
    if variables
        .iter()
        .any(|(key, _)| named(key, "PROMPT_COMMAND"))
    {
        return None;
    }
    let mut wslenv = OsString::new();
    variables.retain(|(key, value)| {
        if named(key, "WSLENV") {
            wslenv = value.clone();
            false
        } else {
            true
        }
    });
    if wslenv.to_string_lossy().split(':').any(|entry| {
        entry
            .split('/')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("PROMPT_COMMAND"))
    }) {
        return None;
    }
    if !wslenv.is_empty() {
        wslenv.push(":");
    }
    wslenv.push("PROMPT_COMMAND/u");
    // Stop exporting the hook after the first prompt. A nested Bash is a
    // foreground command of this shell and must not report it as idle.
    let hook = r"export -n PROMPT_COMMAND; if [[ ${PS0-} != *$'\e]133;C\a'* ]]; then PS0=$'\e]133;C\a'${PS0-}; fi; printf '\033]133;A\007'";
    variables.extend([
        (OsString::from("WSLENV"), wslenv),
        (OsString::from("PROMPT_COMMAND"), OsString::from(hook)),
    ]);
    variables.sort_by_cached_key(|(key, _)| key.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (key, value) in variables {
        block.extend(
            key.encode_wide()
                .chain([b'=' as u16])
                .chain(value.encode_wide())
                .chain([0]),
        );
    }
    block.push(0);
    Some(block)
}

pub(crate) fn publish_profile_names(window: &AppWindow) {
    let default_id = default_shell_id(window);
    window.set_shell_profile_names(ModelRc::new(VecModel::from(
        configured_shells(window)
            .into_iter()
            .map(|shell| {
                let mut label = shell.name.clone();
                if shell.id == default_id {
                    label.push_str(&format!(" ({})", pick("既定", "Default")));
                }
                if let Some(reason) = unavailable_reason(window, &shell) {
                    label.push_str(&format!(" ({reason})"));
                }
                SharedString::from(label)
            })
            .collect::<Vec<_>>(),
    )));
}

fn publish(window: &AppWindow, index: usize) {
    publish_profile_names(window);
    let shells = configured_shells(window);
    let default_id = default_shell_id(window);
    let index = index.min(shells.len().saturating_sub(1));
    window.set_shell_profile_index(index as i32);
    window.set_shell_profile_default(shells.get(index).is_some_and(|s| s.id == default_id));
    if let Some(shell) = shells.get(index) {
        fill_profile(window, &shell.name, &shell.command, &shell.directory);
    }
}

fn fill_profile(window: &AppWindow, name: &str, command: &str, directory: &str) {
    let (exe, args) = command_parts(command);
    window.set_shell_profile_name(name.into());
    window.set_shell_profile_executable(exe.into());
    window.set_shell_profile_arguments(args.into());
    window.set_shell_profile_directory(directory.into());
}

fn draft_profile(window: &AppWindow, name: &str, command: &str, directory: &str) {
    window.set_shell_profile_index(-1);
    window.set_shell_profile_default(false);
    fill_profile(window, name, command, directory);
}

pub(crate) fn action(window: &AppWindow, live: &Live, what: i32, at: i32) {
    let mut shells = configured_shells(window);
    let mut index = usize::try_from(window.get_shell_profile_index())
        .unwrap_or(shells.len())
        .min(shells.len());
    let editing_saved = index < shells.len();
    let old_default = default_shell_id(window);
    // RFN01-169: 入力値には秘密情報が含まれる場合がある。操作と ID だけを記録する。
    live.cache.borrow_mut().log_diag(
        "spec.shell",
        &format!(
            "profile action={what} at={at} id={:?} count={}",
            shells.get(index).map(|shell| shell.id),
            shells.len(),
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
            let profile = TerminalShell {
                id: shells.get(index).map_or(0, |shell| shell.id),
                name,
                command: format!("\"{exe}\" {args}").trim().to_owned(),
                directory,
            };
            if editing_saved {
                shells[index] = profile;
            } else {
                shells.push(profile);
            }
        }
        2 => {
            draft_profile(
                window,
                &format!("Shell {}", shells.len() + 1),
                "powershell.exe -NoLogo",
                "",
            );
            return;
        }
        3 if editing_saved => {
            let source = &shells[index];
            draft_profile(
                window,
                &format!("{} ({})", source.name, shells.len() + 1),
                &source.command,
                &source.directory,
            );
            return;
        }
        4 if editing_saved && index > 0 => {
            shells.swap(index, index - 1);
            index -= 1;
        }
        5 if index + 1 < shells.len() => {
            shells.swap(index, index + 1);
            index += 1;
        }
        6 if editing_saved && shells.len() > 1 => {
            shells.remove(index);
            index = index.min(shells.len() - 1);
        }
        7 if editing_saved => {}
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
        9 => {
            let Some(distribution) = usize::try_from(at)
                .ok()
                .and_then(|at| window.get_wsl_distributions().row_data(at))
            else {
                return;
            };
            if let Some(existing) = shells.iter().position(|shell| {
                wsl_distribution(&shell.command)
                    .flatten()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&distribution))
            }) {
                publish(window, existing);
                return;
            }
            let base = format!("WSL ({distribution})");
            let name = std::iter::once(base.clone())
                .chain((2..).map(|number| format!("{base} ({number})")))
                .find(|name| shells.iter().all(|shell| shell.name != *name))
                .unwrap();
            draft_profile(
                window,
                &name,
                &format!("wsl.exe --distribution {distribution}"),
                "",
            );
            return;
        }
        _ => return,
    }
    let wanted = if what == 7 {
        shells[index].id
    } else {
        old_default
    };
    hold_shells(window, &shells);
    let wanted = if shells.iter().any(|shell| shell.id == wanted) {
        wanted.to_string()
    } else {
        String::new()
    };
    window.set_default_shell_id(wanted.into());
    publish_shells(window);
    publish(window, index);
    save_settings(window, &live.cache);
    live.cache.borrow_mut().log_diag(
        "spec.shell",
        &format!("profile saved action={what} index={index}"),
    );
}

pub(crate) fn open_in(window: &AppWindow, live: &Live, directory: PathBuf) {
    let mut shell = default_shell(window);
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

    #[test]
    fn launch_repairs_a_saved_quoted_wsl_distribution() {
        let shell = TerminalShell {
            id: 4,
            name: "Ubuntu".into(),
            command: r#"wsl.exe -d "Ubuntu" --exec echo "keep quotes""#.into(),
            directory: "~".into(),
        };
        assert_eq!(
            launch(&shell),
            (
                r#""wsl.exe" --cd "~" -d Ubuntu --exec echo "keep quotes""#.into(),
                None
            )
        );
        let shell = TerminalShell {
            command: r#"wsl.exe --exec echo --distribution "Ubuntu""#.into(),
            directory: String::new(),
            ..shell
        };
        assert_eq!(launch(&shell).0, shell.command);
    }

    #[test]
    #[ignore = "starts the installed WSL distributions; run explicitly"]
    fn launch_starts_each_registered_wsl_distribution_from_saved_profile() {
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        let distributions = installed_distributions();
        assert!(!distributions.is_empty());
        for distribution in distributions {
            let shell = TerminalShell {
                id: 4,
                name: distribution.clone(),
                command: format!("wsl.exe --distribution \"{distribution}\" --exec /bin/true"),
                directory: "~".into(),
            };
            let (command, _) = launch(&shell);
            let (program, args) = command_parts(&command);
            let output = Command::new(program).raw_arg(args).output().unwrap();
            assert!(
                output.status.success(),
                "{distribution}: {:?}",
                output.status
            );
        }
    }

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
        for separator in ["--", "-e", "--exec"] {
            assert_eq!(
                wsl_distribution(&format!("wsl.exe {separator} bash -d x")),
                Some(None),
                "{separator} ends WSL options"
            );
        }
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
        use std::process::Command;

        struct Fixture(String);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = Command::new("wsl.exe")
                    .args(["-e", "rm", "-f", "--", &format!("{}/windows", self.0)])
                    .status();
                let _ = Command::new("wsl.exe")
                    .args(["-e", "rmdir", "--", &self.0])
                    .status();
            }
        }

        let made = Command::new("wsl.exe")
            .args(["-e", "mktemp", "-d", "/tmp/rfn-link-test.XXXXXXXXXX"])
            .output()
            .unwrap();
        assert!(made.status.success());
        let path = String::from_utf8(made.stdout).unwrap();
        let path = path.trim();
        assert!(path.starts_with("/tmp/rfn-link-test.") && !path.contains('\n'));
        let fixture = Fixture(path.to_owned());
        let link = format!("{}/windows", fixture.0);
        assert!(
            Command::new("wsl.exe")
                .args(["-e", "ln", "-s", "--", "/mnt/c/Windows", &link])
                .status()
                .unwrap()
                .success()
        );
        let system = format!("{link}/System32");
        let answer = ask_wsl(None, &system);
        assert_eq!(
            answer.map(|p| p.to_string_lossy().to_ascii_lowercase()),
            Some("c:\\windows\\system32".to_owned())
        );
        assert_eq!(ask_wsl(None, "~"), None, "the home is WSL's own");
        // Not asked yet and not waited for: nothing now, the answer arrives.
        let (told, heard) = std::sync::mpsc::channel();
        assert_eq!(
            cached_wsl_windows_path(None, &link, move || {
                let _ = told.send(());
            }),
            None
        );
        heard
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(cached_wsl_windows_path(None, &link, || {}).is_some());
        assert!(
            Command::new("wsl.exe")
                .args(["-e", "ln", "-sfn", "--", "/mnt/c/Windows/System32", &link])
                .status()
                .unwrap()
                .success()
        );
        let (told, heard) = std::sync::mpsc::channel();
        assert!(start_fresh_wsl_lookup(move || {
            told.send(ask_wsl(None, &link)).unwrap();
        }));
        let now = heard.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            now.map(|path| path.to_string_lossy().to_ascii_lowercase()),
            Some("c:\\windows\\system32".into())
        );
    }

    #[test]
    fn wsl_hover_cache_bounds_size_and_expires_per_distribution() {
        let mut cache = Lookups::default();
        let now = Instant::now();
        let failed = (Some("ubuntu".to_owned()), "/same".to_owned());
        let worked = (Some("debian".to_owned()), "/same".to_owned());
        let path = PathBuf::from("D:\\Debian");
        cache.remember(failed.clone(), None, now);
        cache.remember(worked.clone(), Some(path.clone()), now);
        assert_eq!(cache.known[&failed].1, None);
        assert_eq!(cache.known[&worked].1, Some(path.clone()));
        cache.remember(
            (None, "/same".into()),
            None,
            now + Duration::from_millis(300),
        );
        assert!(!cache.known.contains_key(&failed));
        assert!(cache.known.contains_key(&worked));
        for index in 0..WSL_LOOKUP_ENTRIES {
            cache.remember((None, format!("/{index}")), Some(path.clone()), now);
        }
        assert_eq!(cache.known.len(), WSL_LOOKUP_ENTRIES);
        cache.remember((None, "/again".into()), None, now + Duration::from_secs(3));
        assert_eq!(cache.known.len(), 1);
    }

    #[test]
    fn quoted_program_and_japanese_directory_roundtrip() {
        let shell = TerminalShell {
            id: 0,
            name: "My Shell".into(),
            command: "\"C:\\Program Files\\PowerShell\\pwsh.exe\" -NoLogo".into(),
            directory: "D:\\原稿 作業".into(),
        };
        assert_eq!(shell.program(), "C:\\Program Files\\PowerShell\\pwsh.exe");
        assert_eq!(TerminalShell::read(&shell.written()), Some(shell.clone()));
        let (command, directory) = launch(&shell);
        assert!(command.starts_with(&format!("{} -NoExit -Command ", shell.command)));
        assert_eq!(directory, Some(PathBuf::from(&shell.directory)));
        let mut scripted = shell.clone();
        scripted.command.push_str(" -File script.ps1");
        assert_eq!(launch(&scripted).0, scripted.command);
        let linux = TerminalShell {
            id: 0,
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
            id: 0,
            name: "PowerShell".into(),
            command: "powershell.exe -NoLogo".into(),
            directory: String::new(),
        };
        let wsl = TerminalShell {
            id: 0,
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
