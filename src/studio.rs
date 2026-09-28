//! Finding, launching and stopping Roblox Studio.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
};

use anyhow::{Context, bail};

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

    pub fn launch(&self, place: &Path) -> anyhow::Result<StudioProcess> {
        let mut command = Command::new(&self.executable);
        command
            .arg(place)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

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
        Ok(StudioProcess(child))
    }
}

/// A running Studio that is shut down when this guard is dropped.
pub struct StudioProcess(Child);

impl StudioProcess {
    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.0.try_wait().ok().flatten()
    }
}

impl Drop for StudioProcess {
    fn drop(&mut self) {
        if self.try_wait().is_some() {
            return;
        }

        // Studio would prompt about unsaved changes if asked to quit nicely,
        // and the place is a throwaway copy anyway.
        let _ = self.0.kill();
        let _ = self.0.wait();
        crate::debug!("stopped Studio");
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

    const APP_NAME: &str = "RobloxStudio.app";

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
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    const EXECUTABLE_NAME: &str = "RobloxStudioBeta.exe";

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
