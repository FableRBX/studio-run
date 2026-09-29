//! Finding, launching and stopping Roblox Studio.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, bail};

/// How often a hidden Studio is checked, and hidden again if it has shown
/// itself.
const HIDE_INTERVAL: Duration = Duration::from_millis(20);

pub struct Studio {
    pub executable: PathBuf,
    pub plugins_dir: PathBuf,
}

impl Studio {
    pub fn locate(executable: Option<&Path>, plugins_dir: Option<&Path>) -> anyhow::Result<Self> {
        let executable = match executable {
            Some(path) => resolve_executable(path)?,
            None => platform::default_executable()?,
        };

        let plugins_dir = match plugins_dir {
            Some(path) => path.to_owned(),
            None => {
                match platform::settings_dir().and_then(|dir| plugins_dir_from_settings(&dir)) {
                    Some(path) => path,
                    None => platform::default_plugins_dir()?,
                }
            }
        };

        crate::debug!("using Studio at {}", executable.display());
        crate::debug!("using plugins folder {}", plugins_dir.display());
        Ok(Self {
            executable,
            plugins_dir,
        })
    }

    pub fn launch(&self, place: &Path, hidden: bool) -> anyhow::Result<StudioProcess> {
        let mut command = Command::new(&self.executable);
        command
            .arg(place)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Qt apps bring themselves to the front as soon as they finish
        // launching, which would steal focus before Studio can be hidden.
        #[cfg(target_os = "macos")]
        if hidden {
            command.env("QT_MAC_DISABLE_FOREGROUND_APPLICATION_TRANSFORM", "1");
        }

        // Keep Ctrl+C in the terminal from reaching Studio directly; studio-run
        // shuts it down itself.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        let child = command.spawn().with_context(|| {
            format!(
                "Could not launch Roblox Studio at {}",
                self.executable.display()
            )
        })?;

        crate::debug!("launched Studio (pid {})", child.id());

        let hider = if !hidden {
            None
        } else if platform::CAN_HIDE {
            Some(Hider::start(child.id()))
        } else {
            crate::warning!("--hidden only works on macOS and Windows, so Studio stays visible");
            None
        };

        Ok(StudioProcess { child, hider })
    }
}

/// A running Studio that is shut down when this guard is dropped.
pub struct StudioProcess {
    child: Child,
    hider: Option<Hider>,
}

impl StudioProcess {
    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for StudioProcess {
    fn drop(&mut self) {
        // Stop hiding before Studio's process ID can be reused.
        drop(self.hider.take());

        if self.try_wait().is_some() {
            return;
        }

        // Studio would prompt about unsaved changes if asked to quit nicely,
        // and the place is a throwaway copy anyway.
        let _ = self.child.kill();
        let _ = self.child.wait();
        crate::debug!("stopped Studio");
    }
}

/// Keeps Studio out of sight until dropped. Studio brings itself to the front
/// several times while it opens a place, so hiding it once isn't enough.
struct Hider {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Hider {
    fn start(pid: u32) -> Self {
        let (stop, stopped) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut showing = false;
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(HIDE_INTERVAL) {
                let was_showing = showing;
                showing = platform::hide(pid);
                if showing && !was_showing {
                    crate::debug!("hiding Studio");
                }
            }
        });

        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Hider {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Accepts either the Studio executable or, on macOS, the app bundle.
fn resolve_executable(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_dir() {
        let inside_bundle = path.join("Contents").join("MacOS").join("RobloxStudio");
        if inside_bundle.is_file() {
            return Ok(inside_bundle);
        }
        bail!(
            "{} is a folder, not the Roblox Studio executable",
            path.display()
        );
    }

    if !path.is_file() {
        bail!("Roblox Studio was not found at {}", path.display());
    }

    Ok(path.to_owned())
}

/// Studio lets users move their plugins folder, and records the choice as
/// `<QDir name="PluginsDir">` in its settings file.
fn plugins_dir_from_settings(settings_dir: &Path) -> Option<PathBuf> {
    // Settings files are versioned (GlobalSettings_13.xml); use the newest.
    let settings_file = fs::read_dir(settings_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let version: u32 = name
                .to_str()?
                .strip_prefix("GlobalSettings_")?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)?
        .1;

    parse_plugins_dir(&fs::read_to_string(settings_file).ok()?)
}

fn parse_plugins_dir(settings: &str) -> Option<PathBuf> {
    const START: &str = "<QDir name=\"PluginsDir\">";

    let value = settings
        .split_once(START)?
        .1
        .split_once("</QDir>")?
        .0
        .trim();
    if value.is_empty() {
        return None;
    }

    let value = value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    Some(PathBuf::from(value))
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::PathBuf;

    use anyhow::{Context, bail};
    use objc2_app_kit::NSRunningApplication;

    const APP_NAME: &str = "RobloxStudio.app";

    pub const CAN_HIDE: bool = true;

    /// Hides Studio the way Cmd+H does. Returns whether it was showing.
    pub fn hide(pid: u32) -> bool {
        objc2::rc::autoreleasepool(|_| {
            // Not found until Studio has registered with the window server.
            let Some(app) =
                NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32)
            else {
                return false;
            };
            // `hide` returns false even when it works, so whether Studio was
            // showing comes from `isHidden` instead.
            let showing = !app.isHidden();
            if showing {
                app.hide();
            }
            showing
        })
    }

    pub fn default_executable() -> anyhow::Result<PathBuf> {
        let mut candidates = vec![PathBuf::from("/Applications").join(APP_NAME)];
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join("Applications").join(APP_NAME));
        }

        for app in &candidates {
            let executable = app.join("Contents").join("MacOS").join("RobloxStudio");
            if executable.is_file() {
                return Ok(executable);
            }
        }

        bail!(
            "Could not find Roblox Studio in /Applications or ~/Applications. \
             Install it, or pass --studio with the path to RobloxStudio.app."
        )
    }

    pub fn default_plugins_dir() -> anyhow::Result<PathBuf> {
        let documents = dirs::document_dir().context("Could not find your Documents folder")?;
        Ok(documents.join("Roblox").join("Plugins"))
    }

    pub fn settings_dir() -> Option<PathBuf> {
        Some(dirs::home_dir()?.join("Library").join("Roblox"))
    }
}

#[cfg(windows)]
mod platform {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::SystemTime,
    };

    use anyhow::{Context, bail};
    use windows_sys::{
        Win32::{
            Foundation::{HWND, LPARAM, TRUE},
            UI::WindowsAndMessaging::{
                EnumWindows, GW_OWNER, GetForegroundWindow, GetWindow, GetWindowThreadProcessId,
                IsIconic, IsWindowVisible, SW_MINIMIZE, SW_SHOWMINNOACTIVE, ShowWindow,
            },
        },
        core::BOOL,
    };
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    const EXECUTABLE_NAME: &str = "RobloxStudioBeta.exe";

    pub const CAN_HIDE: bool = true;

    /// Minimizes Studio's top-level windows that are showing. Returns whether
    /// there were any.
    pub fn hide(pid: u32) -> bool {
        struct Search {
            pid: u32,
            minimized: bool,
        }

        unsafe extern "system" fn visit(window: HWND, search: LPARAM) -> BOOL {
            // SAFETY: `search` is the `&mut Search` passed to `EnumWindows`
            // below, which only calls back before it returns.
            let search = unsafe { &mut *(search as *mut Search) };

            let mut owner_pid = 0;
            // SAFETY: `window` came from `EnumWindows`. If it has closed
            // since, these calls fail harmlessly.
            unsafe {
                GetWindowThreadProcessId(window, &mut owner_pid);
                // Dialogs and tool windows are owned by the main window and
                // are minimized along with it.
                if owner_pid == search.pid
                    && IsWindowVisible(window) != 0
                    && IsIconic(window) == 0
                    && GetWindow(window, GW_OWNER).is_null()
                {
                    // Hand focus back if Studio took it. Otherwise leave focus
                    // where it is.
                    let show = if GetForegroundWindow() == window {
                        SW_MINIMIZE
                    } else {
                        SW_SHOWMINNOACTIVE
                    };
                    ShowWindow(window, show);
                    search.minimized = true;
                }
            }
            TRUE
        }

        let mut search = Search {
            pid,
            minimized: false,
        };
        // SAFETY: `visit` matches `WNDENUMPROC` and only uses `search` for the
        // duration of the call.
        unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
        search.minimized
    }

    pub fn default_executable() -> anyhow::Result<PathBuf> {
        if let Some(executable) = from_registry() {
            return Ok(executable);
        }

        if let Some(executable) = newest_in_versions_folders() {
            return Ok(executable);
        }

        bail!(
            "Could not find a Roblox Studio installation. \
             Install it, or pass --studio with the path to {EXECUTABLE_NAME}."
        )
    }

    pub fn default_plugins_dir() -> anyhow::Result<PathBuf> {
        let local_app_data = dirs::data_local_dir().context("Could not find %LOCALAPPDATA%")?;
        Ok(local_app_data.join("Roblox").join("Plugins"))
    }

    pub fn settings_dir() -> Option<PathBuf> {
        Some(dirs::data_local_dir()?.join("Roblox"))
    }

    /// Checks the places the installer and Studio record the current version.
    /// Any of them can be missing or point at a version that has since been
    /// removed, so each is only trusted if the executable is really there.
    fn from_registry() -> Option<PathBuf> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let read = |key: &str, value: &str| -> Option<String> {
            hkcu.open_subkey(key).ok()?.get_value(value).ok()
        };

        let client_exe = read(
            r"Software\ROBLOX Corporation\Environments\roblox-studio",
            "clientExe",
        )
        .map(PathBuf::from);

        let install_location = read(
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall\roblox-studio",
            "InstallLocation",
        )
        .map(|folder| PathBuf::from(folder).join(EXECUTABLE_NAME));

        // Written by Studio itself, as the `content` folder inside the version.
        let content_folder = read(r"Software\Roblox\RobloxStudio", "ContentFolder")
            .and_then(|folder| Some(Path::new(&folder).parent()?.join(EXECUTABLE_NAME)));

        [client_exe, install_location, content_folder]
            .into_iter()
            .flatten()
            .find(|executable| executable.is_file())
    }

    /// Falls back to the most recently installed version folder. Player
    /// versions live alongside Studio's, so only folders containing Studio
    /// count.
    fn newest_in_versions_folders() -> Option<PathBuf> {
        let versions = dirs::data_local_dir()?.join("Roblox").join("Versions");

        fs::read_dir(versions)
            .ok()?
            .flatten()
            .filter_map(|entry| existing_executable(&entry.path()))
            .max_by_key(|executable| modified(executable))
    }

    fn existing_executable(version_folder: &Path) -> Option<PathBuf> {
        let executable = version_folder.join(EXECUTABLE_NAME);
        executable.is_file().then_some(executable)
    }

    fn modified(path: &Path) -> SystemTime {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use std::path::PathBuf;

    use anyhow::bail;

    pub const CAN_HIDE: bool = false;

    pub fn hide(_pid: u32) -> bool {
        false
    }

    pub fn default_executable() -> anyhow::Result<PathBuf> {
        bail!(
            "Roblox Studio is only available on Windows and macOS. Pass --studio to point at a launcher."
        )
    }

    pub fn default_plugins_dir() -> anyhow::Result<PathBuf> {
        bail!("Pass --plugins-dir with the plugins folder your Studio launcher uses.")
    }

    pub fn settings_dir() -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_plugins_folder_from_studio_settings() {
        let settings = r#"<roblox><Item class="Studio"><Properties>
            <QDir name="LocalAssetsFolder">/Users/me/Documents/Roblox/LocalAssets</QDir>
            <QDir name="PluginsDir">/Volumes/Work/Roblox &amp; Friends/Plugins</QDir>
        </Properties></Item></roblox>"#;

        assert_eq!(
            parse_plugins_dir(settings),
            Some(PathBuf::from("/Volumes/Work/Roblox & Friends/Plugins"))
        );
    }

    #[test]
    fn ignores_a_blank_or_missing_plugins_folder_setting() {
        assert_eq!(
            parse_plugins_dir(r#"<QDir name="PluginsDir"></QDir>"#),
            None
        );
        assert_eq!(
            parse_plugins_dir(r#"<QDir name="IconOverrideDir">/x</QDir>"#),
            None
        );
    }

    #[test]
    fn uses_the_newest_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let setting = |path: &str| format!(r#"<QDir name="PluginsDir">{path}</QDir>"#);
        fs::write(dir.path().join("GlobalSettings_9.xml"), setting("/old")).unwrap();
        fs::write(dir.path().join("GlobalSettings_13.xml"), setting("/new")).unwrap();
        fs::write(
            dir.path().join("GlobalBasicSettings_14.xml"),
            setting("/basic"),
        )
        .unwrap();

        assert_eq!(
            plugins_dir_from_settings(dir.path()),
            Some(PathBuf::from("/new"))
        );
    }
}
