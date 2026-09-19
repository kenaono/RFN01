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

pub(crate) fn launch(shell: &TerminalShell) -> (String, Option<PathBuf>) {
    let cwd = shell.directory.trim();
    let (program, args) = command_parts(&shell.command);
    let wsl = Path::new(program)
        .file_stem()
        .is_some_and(|p| p.to_string_lossy().eq_ignore_ascii_case("wsl"));
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
}
