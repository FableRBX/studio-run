# studio-run

Run a Luau script inside Roblox Studio and stream its output back to your terminal.

studio-run opens a temporary copy of a place in Studio and runs your script with plugin
permissions. Everything Studio logs is printed to stdout, and Studio closes when the script
finishes. The exit code tells you whether the script succeeded, so test runners, linters and
other automation built on Studio work in a shell or on CI.

It is a ground-up rebuild of [run-in-roblox](https://github.com/rojo-rbx/run-in-roblox), which
has been unmaintained since 2020. It runs natively on Apple Silicon and Intel Macs and on
Windows.

## Installation

### From source

You need [Rust](https://rustup.rs) 1.85 or newer.

```sh
cargo install --path .
```

### Prebuilt binaries

Pushing a tag like `v0.1.0` runs the release workflow. It publishes binaries for macOS (Apple
Silicon and Intel) and Windows (x64 and ARM64) to a draft GitHub release. The assets are named
`studio-run-<version>-<os>-<arch>.zip`, which [Rokit](https://github.com/rojo-rbx/rokit),
Aftman and Foreman all understand.

## Usage

```sh
studio-run --place Game.rbxl --script run-tests.luau
```

This opens a copy of `Game.rbxl` in Studio, runs `run-tests.luau` until it returns, prints
everything Studio logged in the meantime, and closes Studio. `--place` is optional; without
it, the script runs in an empty place.

```sh
# Pass arguments to the script. It receives them as `...`
studio-run --script check.luau -- --strict src/

# Read the script from stdin
echo 'print(workspace.Gravity)' | studio-run --script -
```

The script runs as a `ModuleScript` inside a plugin, so it can use any API a plugin can, and the
`plugin` object is available as `plugin`. It may yield (`task.wait`, HTTP requests, and so on), and
the run ends when the script returns.

Errors point at your file, so terminals and editors can link to the line:

```
$ studio-run --script runtime-error.luau
before
runtime-error.luau:3: kaboom
runtime-error.luau:3
```

### Options

| Option | Description |
| --- | --- |
| `-s, --script <PATH>` | Luau script to run, or `-` to read it from stdin. Required. |
| `-p, --place <PATH>` | Place file (`.rbxl` or `.rbxlx`) to open. Uses an empty place if omitted. The original file is never modified. |
| `--startup-timeout <SECONDS>` | How long to wait for Studio to open the place and connect. Default: 120. |
| `--timeout <SECONDS>` | How long the script may run before it's stopped. No limit by default. |
| `--script-errors-only` | Only fail if the script itself throws. See [exit codes](#exit-codes). |
| `--port <PORT>` | Local port Studio reports back on. Picks a free port by default. Env: `STUDIO_RUN_PORT`. |
| `--studio <PATH>` | Studio executable, or `RobloxStudio.app` on macOS. Env: `ROBLOX_STUDIO_PATH`. |
| `--plugins-dir <PATH>` | Studio's local plugins folder. Env: `ROBLOX_PLUGINS_PATH`. |
| `-v, --verbose` | Print diagnostics about each step to stderr. |

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | The script finished without errors. |
| 1 | The script threw an error, or something logged an error while it ran. Errors from threads the script spawned count. `--script-errors-only` limits this to errors thrown by the script itself. |
| 2 | studio-run could not complete the run. For example, Studio wasn't found, didn't connect in time, closed early, or the script hit `--timeout`. |
| 130 | The run was interrupted with Ctrl+C. |

## How it works

1. The place is copied into a temporary folder under a unique name, and an empty place is
   generated if none was given.
2. studio-run starts a small HTTP server on `127.0.0.1`.
3. It writes a single-use plugin into Studio's local plugins folder. The folder is created if
   it doesn't exist yet. The plugin contains your script and a secret token for this run.
4. Studio is launched on the place copy. When the plugin loads, it checks in with the server.
   The server only accepts the Studio that opened this run's place copy. Any other Studio that
   opens a place while the run is in progress also loads the plugin. That includes a place you
   open yourself or one from a parallel run. Those are turned away and do nothing.
5. The plugin runs your script and forwards Studio's output (`LogService.MessageOut`) in small
   batches. Then it reports whether the script succeeded.
6. studio-run closes Studio and deletes the plugin and the temporary files. This also happens on
   errors, timeouts and Ctrl+C. If a run is killed before it can clean up, the next run deletes
   its leftover plugin.

Studio is found in these places:

| Platform | Studio | Plugins folder |
| --- | --- | --- |
| macOS | `/Applications/RobloxStudio.app` or `~/Applications/RobloxStudio.app` | `~/Documents/Roblox/Plugins` |
| Windows | The version recorded in the registry, or the newest `RobloxStudioBeta.exe` in `%LOCALAPPDATA%\Roblox\Versions` | `%LOCALAPPDATA%\Roblox\Plugins` |

If you've moved the plugins folder in Studio's settings, that location is used instead. Use
`--studio` and `--plugins-dir` to override either one.

## Running on CI

Studio has to be installed and signed in on the machine that runs studio-run. Studio can't
open places while signed out, and runs then fail with a startup timeout. Self-hosted runners
where Studio is already signed in are the most reliable option.

If runs time out, try `--verbose` and check that no dialog in Studio is waiting for input.

## Migrating from run-in-roblox

- The command is `studio-run`. `--place` and `--script` work the same way.
- `--place` is optional now.
- The port is picked automatically. Use `--port` to pin it.
- New options: `--timeout`, `--startup-timeout`, `--script-errors-only`, script arguments after
  `--`, and scripts from stdin.
- Only the Studio that studio-run launched runs the script. Parallel runs and Studio windows you
  already have open are safe.
- A missing plugins folder is created instead of causing a crash, and a plugins folder moved in
  Studio's settings is respected.

## License

studio-run is available under the MIT License. See [LICENSE](LICENSE) for details.
