//! `krowk host enable`: the daemon as a service of the user's own — a
//! systemd user unit on Linux, a launchd agent on macOS (R-HOST-2) — for a
//! machine that is always on and reached from other devices. Run that way
//! it has no idle window (`KROWK_HOST_IDLE=0`): the service manager starts
//! it at login and again if it dies, and it stays.
//!
//! What is written is a pure function of the binary's path and the
//! environment it needs, so the files are tested as text; the service
//! manager is only ever run by the caller, with the commands `commands`
//! names.
//!
//! A service does not start from a shell, so it has none of the shell's
//! variables: a provider's key the daemon is to use is one `krowk connect`
//! stored, not one exported in a shell profile. `KROWK_HOME`, when set, is
//! carried into the unit so the daemon keeps the same home.

use std::path::{Path, PathBuf};

pub const UNIT: &str = "krowk-host.service";
pub const LABEL: &str = "com.krowk.host";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Systemd,
    Launchd,
}

impl Platform {
    pub fn here() -> Platform {
        if cfg!(target_os = "macos") { Platform::Launchd } else { Platform::Systemd }
    }
}

/// Where the service file goes: `$XDG_CONFIG_HOME/systemd/user/` (else
/// `~/.config/systemd/user/`), or `~/Library/LaunchAgents/`.
pub fn path(platform: Platform, env: &dyn Fn(&str) -> String) -> Result<PathBuf, String> {
    let home = env("HOME");
    if home.is_empty() || !Path::new(&home).is_absolute() {
        return Err("there is no home directory to install the service in — set HOME".into());
    }
    Ok(match platform {
        Platform::Systemd => {
            let config = env("XDG_CONFIG_HOME");
            let base = if !config.is_empty() && Path::new(&config).is_absolute() { PathBuf::from(config) } else { Path::new(&home).join(".config") };
            base.join("systemd/user").join(UNIT)
        }
        Platform::Launchd => Path::new(&home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")),
    })
}

/// The variables the service runs with: no idle exit, and krowk's home
/// when it is not the default.
fn environment(env: &dyn Fn(&str) -> String) -> Vec<(&'static str, String)> {
    let mut vars = vec![("KROWK_HOST_IDLE", "0".to_string())];
    let home = env("KROWK_HOME");
    if !home.is_empty() {
        vars.push(("KROWK_HOME", home));
    }
    vars
}

/// The service file's text.
pub fn render(platform: Platform, exe: &Path, log: &Path, env: &dyn Fn(&str) -> String) -> String {
    match platform {
        Platform::Systemd => systemd(exe, env),
        Platform::Launchd => launchd(exe, log, env),
    }
}

/// A systemd word, quoted: `\` and `"` escaped, and `%` doubled, which
/// systemd would read as a specifier.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

fn systemd(exe: &Path, env: &dyn Fn(&str) -> String) -> String {
    let vars: String = environment(env).iter().map(|(k, v)| format!("Environment={}\n", quote(&format!("{k}={v}")))).collect();
    format!(
        "# Written by `krowk host enable`; `krowk host disable` removes it.\n\
         [Unit]\n\
         Description=krowk host: agent sessions that outlive the terminal\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} host serve\n\
         {vars}\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote(&exe.display().to_string())
    )
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn launchd(exe: &Path, log: &Path, env: &dyn Fn(&str) -> String) -> String {
    let vars: String = environment(env).iter().map(|(k, v)| format!("\t\t<key>{}</key>\n\t\t<string>{}</string>\n", xml(k), xml(v))).collect();
    let (exe, log) = (xml(&exe.display().to_string()), xml(&log.display().to_string()));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by `krowk host enable`; `krowk host disable` removes it. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{exe}</string>\n\
         \t\t<string>host</string>\n\
         \t\t<string>serve</string>\n\
         \t</array>\n\
         \t<key>EnvironmentVariables</key>\n\
         \t<dict>\n\
         {vars}\
         \t</dict>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<dict>\n\
         \t\t<key>SuccessfulExit</key>\n\
         \t\t<false/>\n\
         \t</dict>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>{log}</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// The service manager's commands, in order, once the file is written
/// (`enable`) or before it is removed (`disable`). `uid` is launchd's
/// domain, `gui/<uid>`.
pub fn commands(platform: Platform, enable: bool, file: &Path, uid: u32) -> Vec<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let file = file.display().to_string();
    match (platform, enable) {
        (Platform::Systemd, true) => vec![s(&["systemctl", "--user", "daemon-reload"]), s(&["systemctl", "--user", "enable", "--now", UNIT])],
        (Platform::Systemd, false) => vec![s(&["systemctl", "--user", "disable", "--now", UNIT])],
        (Platform::Launchd, true) => vec![s(&["launchctl", "bootstrap", &format!("gui/{uid}"), &file])],
        (Platform::Launchd, false) => vec![s(&["launchctl", "bootout", &format!("gui/{uid}"), &file])],
    }
}

/// The command that succeeds only once the service is up: `enable` asks
/// it before saying so, since `systemctl enable --now` succeeds for a unit
/// that fails a moment later.
pub fn active(platform: Platform, uid: u32) -> Vec<String> {
    match platform {
        Platform::Systemd => vec!["systemctl".into(), "--user".into(), "is-active".into(), UNIT.into()],
        Platform::Launchd => vec!["launchctl".into(), "print".into(), format!("gui/{uid}/{LABEL}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(k: &str) -> String {
        match k {
            "HOME" => "/home/ada".into(),
            "KROWK_HOME" => "/srv/krowk 100%".into(),
            _ => String::new(),
        }
    }

    #[test]
    fn r_host_2_a_systemd_user_unit_runs_the_daemon_with_no_idle_exit() {
        assert_eq!(path(Platform::Systemd, &env).unwrap(), Path::new("/home/ada/.config/systemd/user/krowk-host.service"));
        let unit = render(Platform::Systemd, Path::new("/opt/my krowk/krowk"), Path::new("/x.log"), &env);
        assert!(unit.contains("\nExecStart=\"/opt/my krowk/krowk\" host serve\n"), "{unit}");
        assert!(unit.contains("\nEnvironment=\"KROWK_HOST_IDLE=0\"\n"), "{unit}");
        // A `%` is a specifier to systemd: doubled, it is a `%`.
        assert!(unit.contains("\nEnvironment=\"KROWK_HOME=/srv/krowk 100%%\"\n"), "{unit}");
        assert!(unit.contains("\nWantedBy=default.target\n"), "{unit}");
        assert_eq!(commands(Platform::Systemd, true, Path::new("/f"), 501)[1], ["systemctl", "--user", "enable", "--now", "krowk-host.service"]);
        let xdg = |k: &str| if k == "XDG_CONFIG_HOME" { "/cfg".into() } else { env(k) };
        assert_eq!(path(Platform::Systemd, &xdg).unwrap(), Path::new("/cfg/systemd/user/krowk-host.service"));
    }

    #[test]
    fn r_host_2_a_launchd_agent_runs_the_daemon_with_no_idle_exit() {
        let file = path(Platform::Launchd, &env).unwrap();
        assert_eq!(file, Path::new("/home/ada/Library/LaunchAgents/com.krowk.host.plist"));
        let plist = render(Platform::Launchd, Path::new("/opt/k&k/krowk"), Path::new("/home/ada/.krowk/host.log"), &env);
        assert!(plist.contains("<string>com.krowk.host</string>"), "{plist}");
        assert!(plist.contains("<string>/opt/k&amp;k/krowk</string>\n\t\t<string>host</string>\n\t\t<string>serve</string>"), "{plist}");
        assert!(plist.contains("<key>KROWK_HOST_IDLE</key>\n\t\t<string>0</string>"), "{plist}");
        assert!(plist.contains("<key>StandardErrorPath</key>\n\t<string>/home/ada/.krowk/host.log</string>"), "{plist}");
        assert_eq!(commands(Platform::Launchd, true, &file, 501), [["launchctl", "bootstrap", "gui/501", file.to_str().unwrap()]]);
        assert_eq!(commands(Platform::Launchd, false, &file, 501)[0][1], "bootout");
        assert_eq!(active(Platform::Launchd, 501), ["launchctl", "print", "gui/501/com.krowk.host"]);
        assert_eq!(active(Platform::Systemd, 501), ["systemctl", "--user", "is-active", "krowk-host.service"]);
    }
}
