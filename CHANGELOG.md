# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `--json`: one JSON object on stdout instead of the human output, for scripts and
  notifications. It carries the counts from the summary line plus `changed` (path, rule,
  new/recreated), `failures` and `warnings` arrays, and prints nothing else. `--visual` and
  `--json` are mutually exclusive.
- `asimeow recommend`: finds build and dependency folders your config doesn't exclude yet,
  using a built-in catalog of 36 project types, and shows how much space each would save.
  Suggestions are split into "likely safe" and "check before adding" (generic names like
  `build`, `dist`, `out`, or folders that git tracks). Package-manager and toolchain caches
  (`~/.npm`, `~/.cargo/registry`, `~/.rustup`, …) are listed separately with their sizes.
  Ends with how to apply the likely-safe ones (`--apply`); `-v` prints a config snippet
  instead. Colored in a terminal. Read-only: it changes nothing.
- `asimeow recommend --apply`: adds the likely-safe suggestions to the config and excludes
  the folders found. The config is edited as text, so comments and layout stay; the edit
  is checked by parsing the result, and the previous version is kept as `config.yaml.bak`.
  With `--dry-run`, it only shows what it would change.
- `--dry-run` is accepted after a subcommand too
- `--visual` (alias `--tui`): a full-screen view of a scan. It shows live progress (with
  an estimate from the last run), space saved (total, this run, change since the last
  run), the excluded folders grouped by area and sorted by size (folding, `n` for only
  changes) and recent activity. It stays open for browsing after the scan; `q` quits
  (also during a scan, which then stops and doesn't save the cache) and prints the
  summary to the normal screen. Without a terminal it falls back to plain lines.
- `--dry-run`: report what would be excluded without changing anything
- In a terminal, a live status line (folders scanned, current folder) and a short summary
- Exit code 1 when a folder could not be excluded
- Space savings: every excluded folder is measured, and the summary shows the total
  excluded, what this run added and the change since the last run
  (`9.3 GB excluded · +668 MB this run · +0.6 GB since last run`)
- Recreated folders: a folder that was excluded before but lost its mark (deleting and
  recreating `node_modules` drops it) is reported as recreated, not as new
- `asimeow stats`: the savings and the 10 largest excluded folders from the last scan,
  without scanning
- A cache in `~/Library/Caches/asimeow/state.json` remembers each excluded folder and its
  size. A cached size is reused while the folder has the same inode and mtime and the size
  is less than 7 days old. Dry runs don't update it.

### Changed
- The repository's `config.yaml` is now `config.example.yaml` (with a generic root), and
  `/config.yaml` is git-ignored. Before, running asimeow from the repository picked up that
  file instead of `~/.config/asimeow/config.yaml`, unless `-c` was given.
- Quiet output: only changes are listed (newly excluded folders, failures, warnings).
  Already-excluded folders and unreadable folders (such as `~/.Trash`) are counted in the
  summary and listed only with `-v`.
- When the output is not a terminal, plain lines end with one
  `summary: folders=… exclusions=… new=… recreated=… failed=… unreadable=…
  excluded_bytes=… new_bytes=… seconds=…` line, which replaces the three "Total …" lines
- The scan engine (`scan` module) no longer prints; it sends events that a reporter
  (`report` module) turns into output
- Much faster scans: on a test tree with 120 build folders, a first run went from 335 s
  to 0.16 s and a repeat run from 2.96 s to 0.15 s. Exclusion marks are read from the
  folder's extended attribute and written with the CoreServices call that `tmutil` uses
  underneath, instead of spawning `tmutil` (about 60 ms per check and about 11 s per
  write). The mark is identical to the one `tmutil addexclusion` writes. Details and
  profiles: `_docs/performance.md`.
- Rules and ignore patterns are compiled once per run instead of once per directory entry
- `exclude` and `include` report failures instead of "already excluded/included";
  `include` says when a path stays excluded through a parent folder or a system rule
- A scan no longer treats folders inside system-excluded locations (such as `/tmp`) or
  inside an excluded parent as already excluded: they get their own mark
- New dependencies: `serde_json` (cache file) and `ratatui` (the `--visual` screen)
- Internal: the parallel directory walk moved into its own `walk` module, so other commands can reuse it

### Removed
- Library API (the crate's `explorer` module), replaced by the `scan`, `report`, `tm`,
  `rules`, `cache` and `tui` modules:
  - `run_explorer`, `run_explorer_with_stats`, `ExplorerStats`, `State`, `process_path`,
    `run_workers` → `scan::scan` (events, `Progress`, `Summary`) and `report::run`
  - `is_excluded_from_timemachine` → `tm::is_excluded`; `exclude_from_timemachine` and
    `include_in_timemachine` → `tm::set_exclusion_mark`
  - `config::load_config` no longer prints "Using configuration: …" (the binary does)

### Fixed
- `-c`, `-v` and `-t` are accepted after a subcommand too (`asimeow recommend -c …`)
- A race in the worker queue could end a scan early and silently skip folders
- Symlinked directories are no longer followed (no more loops or walks into other volumes)
- A failed exclusion was reported as "already excluded"; it's now reported as an error

## [0.2.1]

### Fixed
- Avoid traversing already excluded directories

### Added
- Justfile with minimal tasks: `format`, `lint`, `check`, `build`
- Added agents context and flows

## [0.1.9] - 2025-03-24

### Added
- Homebrew installation support
- Automated Homebrew formula updates via GitHub Actions

## [0.1.3] - 2024-11-16

### Fixed
- publish to cargo and github

## [0.1.2] - 2024-11-16

### Added
- Automated GitHub releases with pre-built binaries for macOS (Intel and Apple Silicon)
- Cross-platform binary distribution via GitHub releases

### Fixed
- publish to cargo registry

## [0.1.1] - 2024-11-16

### Added
- GitHub Actions CI/CD pipeline for automated testing and deployment
- Automated publishing to crates.io on new version tags
- Improved documentation with badges and contribution guidelines
- Support for the ignore setting to enhance directory exclusion capabilities.

### Fixed
- Bug in the exploration of excluded folders, ensuring directories specified in the ignore settings are properly excluded.

## [0.1.0] - 2023-11-15

### Added
- Initial release
- Support for recursive directory exploration
- Rule-based detection of project files
- Automatic Time Machine exclusion management
- Multi-threaded processing
- Configuration via YAML files