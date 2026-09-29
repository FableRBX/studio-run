//! One run: prepare the place and plugin, launch Studio, relay its output until
//! the script finishes, and clean everything up afterwards.

use std::{
    ffi::OsStr,
    fmt::{self, Write as _},
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, RecvTimeoutError},
    time::{Duration, Instant},
};

use anstyle::{AnsiColor, Style};
use anyhow::{Context, bail};

use crate::{
    plugin::{self, InstalledPlugin, Plugin},
    server::{self, Finish, Level, OutputMessage, Server},
    studio::Studio,
};

const EMPTY_PLACE: &str = "<roblox version=\"4\">\n</roblox>\n";
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub enum Event {
    /// The plugin in the Studio this run launched checked in.
    Connected,
    Output(Vec<OutputMessage>),
    Finished(Finish),
    Interrupted,
}

pub struct Script {
    /// How the script is referred to in output: the path it was read from.
    pub label: String,
    /// Name of the `ModuleScript` that holds the script inside Studio.
    pub module_name: String,
    pub source: String,
}

pub struct Options {
    pub studio: Studio,
    pub place: Option<PathBuf>,
    pub script: Script,
    pub script_args: Vec<String>,
    pub port: u16,
    pub startup_timeout: Duration,
    pub timeout: Option<Duration>,
    pub hidden: bool,
}

pub struct Outcome {
    /// The script ran to completion without throwing.
    pub script_succeeded: bool,
    /// Something wrote an error to Studio's output while the script ran.
    pub error_output: bool,
}

/// The run was cancelled with Ctrl+C.
#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("Interrupted")
    }
}

impl std::error::Error for Interrupted {}

pub fn run(options: Options) -> anyhow::Result<Outcome> {
    let session_id = random_hex(6)?;
    let token = random_hex(16)?;

    // Studio always opens a private copy of the place. The original is never
    // touched, and Studio can't trip over a lock file left next to it.
    let temp_dir = tempfile::Builder::new()
        .prefix("studio-run-")
        .tempdir()
        .context("Could not create a temporary folder")?;
    let place_path = prepare_place(options.place.as_deref(), temp_dir.path(), &session_id)?;
    let place_file_name = place_path
        .file_name()
        .and_then(OsStr::to_str)
        .expect("place copy has a UTF-8 file name")
        .to_owned();

    let (events_tx, events) = mpsc::channel();

    let interrupt_tx = events_tx.clone();
    ctrlc::set_handler(move || {
        let _ = interrupt_tx.send(Event::Interrupted);
    })
    .context("Could not install the Ctrl+C handler")?;

    let server = Server::start(
        options.port,
        server::Config {
            session_id: session_id.clone(),
            token: token.clone(),
            place_file_name,
            script_args: options.script_args,
        },
        events_tx,
    )?;

    plugin::remove_stale(&options.studio.plugins_dir);

    let plugin = Plugin {
        port: server.port(),
        token: &token,
        script_name: &options.script.module_name,
        script_source: &options.script.source,
    };
    let plugin_file_name = plugin::file_name(server.port(), &session_id);
    let _plugin = InstalledPlugin::install(
        &options.studio.plugins_dir,
        &plugin_file_name,
        &plugin.to_rbxmx(),
    )?;
    let paths = StudioPaths::new(&plugin_file_name, &options.script);

    // Declared last so it is dropped first: Studio has to be gone before its
    // plugin and place copy can be deleted.
    let mut studio = options.studio.launch(&place_path, options.hidden)?;

    let startup_deadline = Instant::now() + options.startup_timeout;
    let mut run_deadline = None;
    let mut connected = false;
    let mut error_output = false;

    loop {
        if let Some(status) = studio.try_wait() {
            bail!("Roblox Studio closed before the script finished ({status})");
        }

        let now = Instant::now();
        if !connected && now >= startup_deadline {
            bail!(startup_timeout_message(
                options.startup_timeout,
                options.hidden
            ));
        }
        if run_deadline.is_some_and(|deadline| now >= deadline) {
            bail!(
                "The script did not finish within {}",
                seconds(options.timeout.unwrap_or_default())
            );
        }

        match events.recv_timeout(POLL_INTERVAL) {
            Ok(Event::Connected) => {
                crate::debug!("Studio connected, running {}", options.script.label);
                connected = true;
                run_deadline = options.timeout.map(|timeout| Instant::now() + timeout);
            }
            Ok(Event::Output(messages)) => {
                for message in &messages {
                    error_output |= message.level == Level::Error;
                    print_output(message.level, &paths.tidy(&message.body));
                }
            }
            Ok(Event::Finished(finish)) => {
                if let Some(error) = &finish.error {
                    print_output(Level::Error, &paths.tidy_traceback(error));
                }

                return Ok(Outcome {
                    script_succeeded: finish.success,
                    error_output,
                });
            }
            Ok(Event::Interrupted) => return Err(Interrupted.into()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("The local server stopped unexpectedly"),
        }
    }
}

fn prepare_place(place: Option<&Path>, dir: &Path, session_id: &str) -> anyhow::Result<PathBuf> {
    let Some(place) = place else {
        let path = dir.join(format!("studio-run-{session_id}.rbxlx"));
        fs::write(&path, EMPTY_PLACE).context("Could not write an empty place")?;
        return Ok(path);
    };

    let extension = place
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase);
    let Some(extension @ ("rbxl" | "rbxlx")) = extension.as_deref() else {
        bail!(
            "{} is not a place file (expected .rbxl or .rbxlx)",
            place.display()
        );
    };

    let path = dir.join(format!("studio-run-{session_id}.{extension}"));
    fs::copy(place, &path)
        .with_context(|| format!("Could not read the place at {}", place.display()))?;
    Ok(path)
}

/// Rewrites Studio's names for the plugin's scripts so output points at the
/// script file instead, like `tests/run.luau:12: oops`.
struct StudioPaths {
    bootstrap: String,
    module: String,
    label: String,
}

impl StudioPaths {
    fn new(plugin_file_name: &str, script: &Script) -> Self {
        let bootstrap = format!("user_{plugin_file_name}.StudioRun");
        let module = format!("{bootstrap}.{}", script.module_name);
        Self {
            bootstrap,
            module,
            label: script.label.clone(),
        }
    }

    fn tidy(&self, text: &str) -> String {
        text.replace(&self.module, &self.label)
    }

    /// Also drops the frames that belong to studio-run's own bootstrap code.
    fn tidy_traceback(&self, traceback: &str) -> String {
        let bootstrap_frame = format!("{}:", self.bootstrap);
        let tidied = self.tidy(traceback);
        let lines: Vec<&str> = tidied
            .lines()
            .filter(|line| !line.trim_start().starts_with(&bootstrap_frame))
            .collect();

        lines.join("\n").trim_end().to_owned()
    }
}

fn print_output(level: Level, body: &str) {
    let style = match level {
        Level::Print => Style::new(),
        Level::Info => AnsiColor::Cyan.on_default(),
        Level::Warning => AnsiColor::Yellow.on_default(),
        Level::Error => AnsiColor::Red.on_default(),
    };

    anstream::println!("{style}{body}{style:#}");
}

fn startup_timeout_message(timeout: Duration, hidden: bool) -> String {
    let see_dialogs = if hidden {
        " Studio was hidden, so run without --hidden to see it."
    } else {
        ""
    };

    format!(
        "Roblox Studio did not connect within {}.\n\n\
         Things to check:\n  \
         - Studio is signed in. Open it once by hand to sign in.\n  \
         - Studio isn't waiting on a dialog, such as a permission prompt for the studio-run plugin.{see_dialogs}\n  \
         - The place opens in Studio without errors.\n\n\
         Use --startup-timeout to wait longer, and --verbose to see what happened.",
        seconds(timeout)
    )
}

fn seconds(duration: Duration) -> String {
    match duration.as_secs() {
        1 => "1 second".to_owned(),
        secs => format!("{secs} seconds"),
    }
}

fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|err| anyhow::anyhow!("Could not generate a session ID: {err}"))?;
    Ok(buffer.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths() -> StudioPaths {
        let script = Script {
            label: "tests/run.luau".to_owned(),
            module_name: "run".to_owned(),
            source: String::new(),
        };
        StudioPaths::new("studio-run-5000-abc.rbxmx", &script)
    }

    #[test]
    fn points_output_at_the_script_file() {
        let paths = test_paths();
        assert_eq!(
            paths.tidy("user_studio-run-5000-abc.rbxmx.StudioRun.run:2: background failure"),
            "tests/run.luau:2: background failure"
        );
        assert_eq!(
            paths.tidy("Script 'user_studio-run-5000-abc.rbxmx.StudioRun.run', Line 2"),
            "Script 'tests/run.luau', Line 2"
        );
        assert_eq!(paths.tidy("user_Rojo.rbxm.Main:1"), "user_Rojo.rbxm.Main:1");
    }

    #[test]
    fn drops_bootstrap_frames_from_tracebacks() {
        let traceback = "user_studio-run-5000-abc.rbxmx.StudioRun.run:3: kaboom\n\
                         user_studio-run-5000-abc.rbxmx.StudioRun.run:3\n\
                         user_studio-run-5000-abc.rbxmx.StudioRun:88\n\
                         user_studio-run-5000-abc.rbxmx.StudioRun:87\n";

        assert_eq!(
            test_paths().tidy_traceback(traceback),
            "tests/run.luau:3: kaboom\ntests/run.luau:3"
        );
    }

    #[test]
    fn copies_places_under_a_unique_name() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("My Game.RBXL");
        fs::write(&source, b"place bytes").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let copy = prepare_place(Some(&source), dir.path(), "abc").unwrap();

        assert_eq!(copy.file_name().unwrap(), "studio-run-abc.rbxl");
        assert_eq!(fs::read(&copy).unwrap(), b"place bytes");
    }

    #[test]
    fn writes_an_empty_place_when_none_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = prepare_place(None, dir.path(), "abc").unwrap();

        assert_eq!(path.file_name().unwrap(), "studio-run-abc.rbxlx");
        assert_eq!(fs::read_to_string(&path).unwrap(), EMPTY_PLACE);
    }

    #[test]
    fn rejects_files_that_are_not_places() {
        let dir = tempfile::tempdir().unwrap();
        let error = prepare_place(Some(Path::new("model.rbxm")), dir.path(), "abc").unwrap_err();
        assert!(error.to_string().contains("not a place file"));
    }
}
