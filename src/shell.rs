//! The shell `!command` runs in. By default your own login shell (`$SHELL`: fish, nushell, zsh, bash...), so what you type
//! after `!` means what it means in your terminal; `/config` → "Shell for !" can pin plain `sh` instead. A shell
//! claude++ does not know how to hand a command to (or one that is not there) falls back to `sh`. The status
//! line command stays on bash/zsh whatever this says: those scripts are written for bash. Claude's own Bash tool
//! uses the shell Claude Code picks (bash or zsh), not this one.

use std::process::Command;

/// Shells that take `-c <command>` (PowerShell takes `-Command`).
const KNOWN: &[&str] = &["bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "ash", "sh", "nu", "elvish", "xonsh", "pwsh"];

/// `command` run by `login` (a path such as `$SHELL`), or by `sh` when that is unknown, missing, or `None`.
pub fn command(login: Option<&str>, command: &str) -> Command {
    let known = login.filter(|p| std::path::Path::new(p).is_file()).and_then(|p| {
        let name = std::path::Path::new(p).file_name()?.to_str()?;
        KNOWN.contains(&name).then(|| (p, name == "pwsh"))
    });
    match known {
        Some((path, powershell)) => {
            let mut c = Command::new(path);
            if powershell {
                c.arg("-NoProfile").arg("-Command");
            } else {
                c.arg("-c");
            }
            c.arg(command);
            c
        }
        None => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(login: Option<&str>, cmd: &str) -> String {
        String::from_utf8_lossy(&command(login, cmd).output().unwrap().stdout).trim().to_string()
    }

    #[test]
    fn the_login_shell_runs_it_and_an_unknown_or_missing_one_falls_back_to_sh() {
        assert_eq!(run(None, "echo $((1+2))"), "3", "sh by default");
        assert_eq!(run(Some("/bin/sh"), "echo ok"), "ok");
        assert_eq!(run(Some("/nonexistent/fish"), "echo ok"), "ok", "missing: sh");
        assert_eq!(run(Some("/bin/ls"), "echo ok"), "ok", "not a shell we know: sh");
        // a shell where the syntax differs proves it is the one running
        if let Some(fish) = ["/opt/homebrew/bin/fish", "/usr/local/bin/fish", "/usr/bin/fish"].into_iter().find(|p| std::path::Path::new(p).is_file()) {
            assert_eq!(run(Some(fish), "set x 5; echo $x"), "5");
            assert_eq!(run(None, "set x 5; echo $x").len(), 0, "and sh would not understand it");
        }
    }
}
