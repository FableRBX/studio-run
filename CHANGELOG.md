# Changelog

## 0.2.0 (2026-09-29)

- `--hidden` keeps Studio out of sight while it runs: hidden on macOS and minimized on Windows.
  Set `STUDIO_RUN_HIDDEN=1` to hide it on every run.

## 0.1.0 (2026-09-28)

This is the first release, rebuilt from scratch as a successor to run-in-roblox.

- Native Apple Silicon and Intel builds for macOS, plus x64 and ARM64 builds for Windows.
- Installable with Rokit (`rokit add FableRBX/studio-run`). Linux builds are included so that
  `rokit install` works on Linux CI runners.
- Runs a script in a copy of a place, or in an empty place when `--place` is omitted.
- Only the Studio this run launched executes the script. Other open Studio windows and parallel
  runs are turned away.
- New options: `--timeout`, `--startup-timeout`, `--script-errors-only`, `--port`,
  `--studio` and `--plugins-dir`.
- Scripts can take arguments (`-- args...`) and be read from stdin (`--script -`).
- Errors point at the script file (`tests/run.luau:12: ...`) instead of Studio's internal plugin
  names.
- Cleans up Studio, the plugin and temporary files on Ctrl+C, and removes plugins left behind by
  runs that were killed.
- Creates Studio's plugins folder if it doesn't exist, and uses the plugins folder set in
  Studio's settings if it has been moved.
