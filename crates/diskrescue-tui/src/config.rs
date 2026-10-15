//! What the app needs to know about the machine it runs on, so that tests
//! can replace it with fixed data (and never touch a real device).

use std::path::{Path, PathBuf};

use diskrescue_core::device::DeviceInfo;

use crate::widgets::Theme;

/// A device that tests supply instead of asking the system.
#[derive(Debug, Clone)]
pub struct FixedDevice {
    /// What the system would report.
    pub info: DeviceInfo,
    /// Reading it needs administrator rights (and the user has none).
    pub needs_root: bool,
}

/// Where the list of devices comes from.
#[derive(Debug, Clone)]
pub enum DeviceSource {
    /// Ask the system (`diskrescue_app::devices::list`).
    System,
    /// A fixed list; no device is ever opened.
    Fixed(Vec<FixedDevice>),
}

/// How "Open folder" works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opener {
    /// The desktop's file manager (`xdg-open`, `open`, `explorer`), unless
    /// running as root or none exists; then the path is shown.
    System,
    /// Never start a program; show the path.
    Disabled,
}

/// How a copy of a device that needs administrator rights is made when the
/// app itself has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Elevation {
    /// Never ask for administrator rights.
    Off,
    /// Ask through `pkexec` (the desktop shows a password prompt).
    Pkexec,
    /// Run this `diskrescue` program as the current user, without asking
    /// for anything (tests of the helper path).
    Program(PathBuf),
}

/// Everything the app reads from its surroundings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Colours or monochrome.
    pub theme: Theme,
    /// The user's home folder (the base of default output folders).
    pub home: PathBuf,
    /// The folder the program was started in.
    pub cwd: PathBuf,
    /// Running with administrator rights.
    pub is_root: bool,
    /// Where the devices come from.
    pub devices: DeviceSource,
    /// How "Open folder" works.
    pub opener: Opener,
    /// How a device that needs administrator rights is copied when the app
    /// has none.
    pub elevation: Elevation,
}

impl Config {
    /// The real machine.
    pub fn detect() -> Config {
        let is_root = running_as_root();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Config {
            theme: Theme::from_env(),
            home: user_home(is_root),
            cwd,
            is_root,
            devices: DeviceSource::System,
            opener: Opener::System,
            // pkexec is Linux only; on Windows the app is started as administrator.
            elevation: if cfg!(windows) {
                Elevation::Off
            } else {
                Elevation::Pkexec
            },
        }
    }

    /// A configuration for tests: fixed devices, nothing opened, no
    /// elevation, colours on.
    pub fn for_tests(home: &Path, devices: Vec<FixedDevice>) -> Config {
        Config {
            theme: Theme::new(true),
            home: home.to_path_buf(),
            cwd: home.to_path_buf(),
            is_root: false,
            devices: DeviceSource::Fixed(devices),
            opener: Opener::Disabled,
            elevation: Elevation::Off,
        }
    }
}

/// True when the program has administrator rights.
pub fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The home folder of the person at the keyboard: under `sudo`, theirs and
/// not root's.
pub fn user_home(is_root: bool) -> PathBuf {
    if is_root {
        if let Some(h) = sudo_user_home() {
            return h;
        }
    }
    for var in ["HOME", "USERPROFILE"] {
        if let Some(h) = std::env::var_os(var) {
            if !h.is_empty() {
                return PathBuf::from(h);
            }
        }
    }
    PathBuf::from(".")
}

fn sudo_user_home() -> Option<PathBuf> {
    let user = std::env::var("SUDO_USER").ok()?;
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    home_from_passwd(&passwd, &user)
}

/// The home folder of `user` in the text of `/etc/passwd`.
pub fn home_from_passwd(passwd: &str, user: &str) -> Option<PathBuf> {
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 6 && f[0] == user && !f[5].is_empty()).then(|| PathBuf::from(f[5]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sudo_users_home_comes_from_the_password_file() {
        let p = "root:x:0:0:root:/root:/bin/bash\nme:x:1000:1000:Me,,,:/home/me:/bin/zsh\n";
        assert_eq!(home_from_passwd(p, "me"), Some(PathBuf::from("/home/me")));
        assert_eq!(home_from_passwd(p, "nobody"), None);
    }
}
