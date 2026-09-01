use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

const SERVICE_NAME: &str = "rexec.service";

pub fn install() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|err| format!("cannot locate the rexec executable: {err}"))?;
    let path = unit_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "systemd user unit path has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    std::fs::write(&path, unit_contents(&executable)?)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;

    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&["--user", "enable", "--now", SERVICE_NAME])?;
    Ok(path)
}

fn unit_path() -> Result<PathBuf, String> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => {
            let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(config.join("systemd/user").join(SERVICE_NAME))
}

fn unit_contents(executable: &Path) -> Result<String, String> {
    let executable = quote_systemd_arg(executable.as_os_str())?;
    Ok(format!(
        "[Unit]\nDescription=rexec per-user command execution host\n\n[Service]\nType=simple\nExecStart={executable} --start-host --silent\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n"
    ))
}

fn quote_systemd_arg(value: &OsStr) -> Result<String, String> {
    let value = value
        .to_str()
        .ok_or_else(|| "rexec executable path is not valid UTF-8".to_string())?;
    if value.chars().any(char::is_control) {
        return Err("rexec executable path contains a control character".to_string());
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    Ok(format!("\"{escaped}\""))
}

fn run_systemctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|err| format!("failed to run systemctl: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        Err(format!(
            "systemctl {} failed with {}",
            args.join(" "),
            output.status
        ))
    } else {
        Err(format!("systemctl {} failed: {detail}", args.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_starts_the_silent_host() {
        let unit = unit_contents(Path::new("/opt/rexec bin/rexec")).unwrap();
        assert!(unit.contains("ExecStart=\"/opt/rexec bin/rexec\" --start-host --silent"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn executable_path_escapes_systemd_specifiers() {
        assert_eq!(
            quote_systemd_arg(OsStr::new("/opt/100%/re\\xec\"bin")).unwrap(),
            "\"/opt/100%%/re\\\\xec\\\"bin\""
        );
    }
}
