use std::{
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anstyle::AnsiColor;
use anyhow::Context;
use clap::{Parser, builder::FalseyValueParser};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Prints a diagnostic line to stderr when `--verbose` is set.
macro_rules! debug {
    ($($arg:tt)*) => {{
        if $crate::verbose() {
            let style = ::anstyle::Style::new().dimmed();
            ::anstream::eprintln!("{style}[studio-run] {}{style:#}", format_args!($($arg)*));
        }
    }};
}
pub(crate) use debug;

macro_rules! warning {
    ($($arg:tt)*) => {{
        let style = ::anstyle::AnsiColor::Yellow.on_default().bold();
        ::anstream::eprintln!("{style}warning:{style:#} {}", format_args!($($arg)*));
    }};
}
pub(crate) use warning;

mod plugin;
mod server;
mod session;
mod studio;

const AFTER_HELP: &str = "\
Examples:
  studio-run --place Game.rbxl --script run-tests.luau
  studio-run --script check.luau -- --some-flag value
  echo 'print(workspace.Gravity)' | studio-run --script -

Exit codes:
  0    The script finished without errors
  1    The script threw an error, or an error was logged while it ran
  2    studio-run could not complete the run
  130  The run was interrupted";

/// Run a Luau script inside Roblox Studio and stream its output to your
/// terminal.
///
/// Studio opens a temporary copy of the place, runs the script with plugin
/// permissions, prints everything Studio logs, and then closes.
#[derive(Debug, Parser)]
#[command(version, after_help = AFTER_HELP)]
struct Cli {
    /// Luau script to run, or `-` to read it from stdin.
    #[arg(short, long, value_name = "PATH")]
    script: PathBuf,

    /// Place file (.rbxl or .rbxlx) to open. Uses an empty place if omitted.
    #[arg(short, long, value_name = "PATH")]
    place: Option<PathBuf>,

    /// Seconds to wait for Studio to open the place and connect.
    #[arg(long, value_name = "SECONDS", default_value_t = 120)]
    startup_timeout: u64,

    /// Seconds the script may run before it's stopped. No limit by default.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,

    /// Only fail if the script itself throws. Normally any error logged while
    /// the script runs (such as one from a thread it spawned) fails the run.
    #[arg(long)]
    script_errors_only: bool,

    /// Keep Studio out of sight while it runs: hidden on macOS, minimized on
    /// Windows. It may flash on screen for a moment while it starts.
    #[arg(long, env = "STUDIO_RUN_HIDDEN", value_parser = FalseyValueParser::new())]
    hidden: bool,

    /// Local port Studio reports back on. Picks a free one by default.
    #[arg(long, env = "STUDIO_RUN_PORT")]
    port: Option<u16>,

    /// Roblox Studio executable, or RobloxStudio.app on macOS. Found
    /// automatically by default.
    #[arg(long, value_name = "PATH", env = "ROBLOX_STUDIO_PATH")]
    studio: Option<PathBuf>,

    /// Studio's local plugins folder. Found automatically by default.
    #[arg(long, value_name = "PATH", env = "ROBLOX_PLUGINS_PATH")]
    plugins_dir: Option<PathBuf>,

    /// Print diagnostics about what studio-run is doing.
    #[arg(short, long)]
    verbose: bool,

    /// Arguments passed to the script, which it receives as `...`.
    #[arg(last = true, value_name = "ARGS")]
    args: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    VERBOSE.store(cli.verbose, Ordering::Relaxed);

    match run(cli) {
        Ok(code) => code,
        Err(err) if err.is::<session::Interrupted>() => ExitCode::from(130),
        Err(err) => {
            let style = AnsiColor::Red.on_default().bold();
            anstream::eprintln!("{style}error:{style:#} {err}");
            for cause in err.chain().skip(1) {
                anstream::eprintln!("  caused by: {cause}");
            }
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let script = read_script(&cli.script)?;
    let studio = studio::Studio::locate(cli.studio.as_deref(), cli.plugins_dir.as_deref())?;

    let outcome = session::run(session::Options {
        studio,
        place: cli.place,
        script,
        script_args: cli.args,
        port: cli.port.unwrap_or(0),
        startup_timeout: Duration::from_secs(cli.startup_timeout),
        timeout: cli.timeout.map(Duration::from_secs),
        hidden: cli.hidden,
    })?;

    let failed = !outcome.script_succeeded || (outcome.error_output && !cli.script_errors_only);
    if failed && outcome.script_succeeded {
        debug!("failing because errors were logged; pass --script-errors-only to ignore them");
    }

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn read_script(path: &Path) -> anyhow::Result<session::Script> {
    if path == Path::new("-") {
        let mut source = String::new();
        std::io::stdin()
            .read_to_string(&mut source)
            .context("Could not read the script from stdin")?;

        return Ok(session::Script {
            label: "stdin".to_owned(),
            module_name: "stdin".to_owned(),
            source,
        });
    }

    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read the script at {}", path.display()))?;
    let module_name = path.file_stem().map_or_else(
        || "script".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );

    Ok(session::Script {
        label: path.display().to_string(),
        module_name,
        source,
    })
}
