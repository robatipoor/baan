use arboard::Clipboard;
use std::{
    env,
    ffi::OsStr,
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};
use tracing::{info, warn};

const RUN_USER_DIR: &str = "/run/user";
const X11_SOCKET_DIR: &str = "/tmp/.X11-unix";
const PASSWD_FILE: &str = "/etc/passwd";

const WAYLAND_DISPLAY_ENV: &str = "WAYLAND_DISPLAY";
const XDG_RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";
const DISPLAY_ENV: &str = "DISPLAY";
const XAUTHORITY_ENV: &str = "XAUTHORITY";

const WAYLAND_DISPLAY_PREFIX: &str = "wayland-";
const XAUTHORITY_FILE: &str = ".Xauthority";
const GDM_XAUTHORITY_PATH: &str = "gdm/Xauthority";

/// Clipboard operations needed by the event loop. Implemented for
/// `arboard::Clipboard`; fakeable in tests.
pub trait ClipboardOps {
    fn get_text(&mut self) -> std::result::Result<String, String>;
    fn set_text(&mut self, text: &str) -> std::result::Result<(), String>;
}

impl ClipboardOps for Clipboard {
    fn get_text(&mut self) -> std::result::Result<String, String> {
        Clipboard::get_text(self).map_err(|e| e.to_string())
    }
    fn set_text(&mut self, text: &str) -> std::result::Result<(), String> {
        Clipboard::set_text(self, text.to_owned()).map_err(|e| e.to_string())
    }
}

#[derive(Debug)]
enum DisplaySession {
    Wayland {
        runtime_dir: PathBuf,
        display: String,
    },
    X11 {
        display: String,
        xauthority: Option<PathBuf>,
    },
}

impl DisplaySession {
    fn configure_environment(&self) {
        match self {
            Self::Wayland {
                runtime_dir,
                display: wayland_display,
            } => {
                set_environment_variable(XDG_RUNTIME_DIR_ENV, runtime_dir);
                set_environment_variable(WAYLAND_DISPLAY_ENV, wayland_display);

                let rd = runtime_dir.display();
                info!(
                    xdg_runtime_dir = %rd,
                    wayland_display = wayland_display.as_str(),
                    "Using discovered Wayland session"
                );
            }

            Self::X11 {
                display: x11_display,
                xauthority,
            } => {
                set_environment_variable(DISPLAY_ENV, x11_display);

                match xauthority {
                    Some(path) => {
                        set_environment_variable(XAUTHORITY_ENV, path);

                        let xa = path.display();
                        info!(
                            display = x11_display.as_str(),
                            xauthority = %xa,
                            "Using discovered X11 session"
                        );
                    }

                    None => {
                        info!(
                            display = x11_display.as_str(),
                            "Using discovered X11 session"
                        );
                    }
                }
            }
        }
    }
}

/// Initializes and returns a clipboard instance.
///
/// The function first tries to use the current display environment.
/// If no valid display environment is found, it attempts to discover
/// an available Wayland or X11 session.
pub fn open() -> Option<Clipboard> {
    Clipboard::new()
        .map_err(|error| {
            warn!(detail = %error, "Failed to initialize clipboard");
        })
        .ok()
}

/// Configures the display environment (sets `DISPLAY`, `WAYLAND_DISPLAY`,
/// `XAUTHORITY`, …).
///
/// Must be called before any threads are spawned: `env::set_var` is unsafe in
/// the 2024 edition once threads exist.
pub fn ensure_display_environment() -> Option<()> {
    if has_valid_wayland_environment() {
        return Some(());
    }

    if has_x11_environment() {
        ensure_xauthority();
        return Some(());
    }

    let session = discover_display_session().or_else(|| {
        warn!("Could not discover any Wayland or X11 display session");
        None
    })?;

    session.configure_environment();

    Some(())
}

fn has_valid_wayland_environment() -> bool {
    let Some(runtime_dir) = env::var_os(XDG_RUNTIME_DIR_ENV) else {
        return false;
    };

    let Some(display) = env::var_os(WAYLAND_DISPLAY_ENV) else {
        return false;
    };

    is_socket_path(&PathBuf::from(runtime_dir).join(display))
}

fn has_x11_environment() -> bool {
    is_environment_variable_set(DISPLAY_ENV)
}

/// Discovers an available display session.
///
/// Wayland is preferred over X11.
fn discover_display_session() -> Option<DisplaySession> {
    discover_wayland_session().or_else(discover_x11_session)
}

fn discover_wayland_session() -> Option<DisplaySession> {
    get_subdirectories(Path::new(RUN_USER_DIR))
        .flat_map(|runtime_dir| {
            find_wayland_sockets(&runtime_dir)
                .map(|display| DisplaySession::Wayland {
                    runtime_dir: runtime_dir.clone(),
                    display,
                })
                .collect::<Vec<DisplaySession>>()
        })
        .min_by(wayland_session_ordering)
}

fn wayland_session_ordering(left: &DisplaySession, right: &DisplaySession) -> std::cmp::Ordering {
    let (
        DisplaySession::Wayland {
            runtime_dir: left_runtime_dir,
            display: left_display,
        },
        DisplaySession::Wayland {
            runtime_dir: right_runtime_dir,
            display: right_display,
        },
    ) = (left, right)
    else {
        unreachable!("Wayland session ordering received a non-Wayland session");
    };

    left_runtime_dir
        .cmp(right_runtime_dir)
        .then_with(|| left_display.cmp(right_display))
}

fn find_wayland_sockets(runtime_dir: &Path) -> impl Iterator<Item = String> {
    read_directory(runtime_dir)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().into_string().ok()?;

            if is_wayland_socket_name(&file_name) && is_socket(&entry) {
                Some(file_name)
            } else {
                None
            }
        })
}

fn is_wayland_socket_name(name: &str) -> bool {
    name.strip_prefix(WAYLAND_DISPLAY_PREFIX)
        .is_some_and(|suffix| suffix.parse::<u32>().is_ok())
}

fn discover_x11_session() -> Option<DisplaySession> {
    read_directory(Path::new(X11_SOCKET_DIR))
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let display_number = parse_x11_display(entry.file_name().as_os_str())?;
            let xauthority = find_xauthority_for_socket(&entry.path());

            Some((display_number, xauthority))
        })
        .min_by_key(|(display_number, _)| *display_number)
        .map(|(display_number, xauthority)| DisplaySession::X11 {
            display: format!(":{display_number}"),
            xauthority,
        })
}

fn parse_x11_display(name: &OsStr) -> Option<u32> {
    let name = name.to_str()?;

    name.strip_prefix('X')?.parse::<u32>().ok()
}

fn ensure_xauthority() {
    if has_valid_xauthority() {
        return;
    }

    let Some(xauthority) = discover_xauthority() else {
        return;
    };

    set_environment_variable(XAUTHORITY_ENV, &xauthority);

    let xa = xauthority.display();
    info!(xauthority = %xa, "Discovered XAUTHORITY");
}

fn discover_xauthority() -> Option<PathBuf> {
    let display = env::var(DISPLAY_ENV).ok()?;
    let display_number = parse_display_number(&display)?;

    let socket_path = Path::new(X11_SOCKET_DIR).join(format!("X{display_number}"));

    find_xauthority_for_socket(&socket_path)
}

fn find_xauthority_for_socket(socket_path: &Path) -> Option<PathBuf> {
    let uid = fs::metadata(socket_path).ok()?.uid();

    let gdm_xauthority = Path::new(RUN_USER_DIR)
        .join(uid.to_string())
        .join(GDM_XAUTHORITY_PATH);

    if gdm_xauthority.is_file() {
        return Some(gdm_xauthority);
    }

    let home_directory = get_home_directory(uid)?;
    let home_xauthority = home_directory.join(XAUTHORITY_FILE);

    home_xauthority.is_file().then_some(home_xauthority)
}

fn parse_display_number(display: &str) -> Option<u32> {
    let display = display.rsplit_once(':')?.1;
    let display = display.split('.').next()?;

    display.parse::<u32>().ok()
}

fn has_valid_xauthority() -> bool {
    env::var_os(XAUTHORITY_ENV)
        .filter(|value| !value.is_empty())
        .is_some_and(|path| Path::new(&path).is_file())
}

fn get_home_directory(uid: u32) -> Option<PathBuf> {
    let passwd_content = fs::read_to_string(PASSWD_FILE).ok()?;

    passwd_content.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();

        if fields.len() < 6 {
            return None;
        }

        // /etc/passwd format:
        // username:password:uid:gid:gecos:home:shell
        let file_uid = fields[2].parse::<u32>().ok()?;

        (file_uid == uid).then(|| PathBuf::from(fields[5]))
    })
}

fn get_subdirectories(directory: &Path) -> impl Iterator<Item = PathBuf> {
    read_directory(directory)
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
}

fn read_directory(directory: &Path) -> impl Iterator<Item = io::Result<fs::DirEntry>> {
    fs::read_dir(directory).into_iter().flatten()
}

fn is_socket(entry: &fs::DirEntry) -> bool {
    entry
        .file_type()
        .is_ok_and(|file_type| file_type.is_socket())
}

fn is_socket_path(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn is_environment_variable_set(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn set_environment_variable<K, V>(key: K, value: V)
where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    // SAFETY:
    // This function must only be called during application initialization,
    // before environment variables are accessed or modified concurrently.
    unsafe {
        env::set_var(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_socket_name_accepts_numeric_suffixes() {
        assert!(is_wayland_socket_name("wayland-0"));
        assert!(is_wayland_socket_name("wayland-1"));
        assert!(is_wayland_socket_name("wayland-10"));
        assert!(is_wayland_socket_name("wayland-00"));
    }

    #[test]
    fn wayland_socket_name_rejects_non_numeric() {
        assert!(!is_wayland_socket_name("wayland-"));
        assert!(!is_wayland_socket_name("wayland-abc"));
        assert!(!is_wayland_socket_name("wayland5")); // missing hyphen
        assert!(!is_wayland_socket_name("wayland-12.5"));
        assert!(!is_wayland_socket_name(""));
        assert!(!is_wayland_socket_name("x11"));
        // Overflowing the u32 range.
        assert!(!is_wayland_socket_name("wayland-99999999999999999999"));
    }

    #[test]
    fn x11_display_parses_numeric_name() {
        for (name, expected) in [("X0", 0), ("X1", 1), ("X12", 12), ("X012", 12)] {
            assert_eq!(parse_x11_display(OsStr::new(name)), Some(expected));
        }
    }

    #[test]
    fn x11_display_rejects_malformed() {
        for name in ["x0", "X", "0", "", "X-1", "Xabc", "Y1"] {
            assert_eq!(
                parse_x11_display(OsStr::new(name)),
                None,
                "unexpected for {name:?}"
            );
        }
    }

    #[test]
    fn display_number_parses_common_forms() {
        for (display, expected) in [(":0", 0), (":0.0", 0), (":1.1", 1), (":42", 42)] {
            let n = parse_display_number(display);
            assert_eq!(n, Some(expected), "display {display:?}");
        }
        // Uses the text after the *last* colon, ignoring a leading label.
        assert_eq!(parse_display_number("abc:0"), Some(0));
    }

    #[test]
    fn display_number_rejects_malformed() {
        for display in ["0", "0.0", ":", ":abc", ":0x", ""] {
            assert_eq!(parse_display_number(display), None, "display {display:?}");
        }
    }

    #[test]
    fn display_number_uses_last_colon_segment() {
        assert_eq!(parse_display_number(":11:22"), Some(22));
    }

    fn wayland(runtime_dir: &str, display: &str) -> DisplaySession {
        DisplaySession::Wayland {
            runtime_dir: PathBuf::from(runtime_dir),
            display: display.to_string(),
        }
    }

    #[test]
    fn wayland_session_ordering_by_runtime_dir_then_display() {
        use std::cmp::Ordering;
        // Lower runtime dir first.
        assert_eq!(
            wayland_session_ordering(&wayland("1000", "wayland-0"), &wayland("1001", "wayland-0")),
            Ordering::Less
        );
        // Same dir: lower display wins.
        assert_eq!(
            wayland_session_ordering(&wayland("1000", "wayland-0"), &wayland("1000", "wayland-1")),
            Ordering::Less
        );
        // Reflection.
        assert_eq!(
            wayland_session_ordering(&wayland("1000", "wayland-1"), &wayland("1000", "wayland-1")),
            Ordering::Equal
        );
    }
}
