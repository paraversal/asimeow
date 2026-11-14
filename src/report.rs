//! Turns scan events into output.
//!
//! - [`LogReporter`]: plain lines for scripts and log files (stdout is not a terminal).
//! - [`CompactReporter`]: a live status line in a terminal, plus one line per change.
//! - [`JsonReporter`]: one JSON object on stdout (`--json`), for scripts that must not
//!   parse the human output.
//!
//! The text reporters print only what changed by default (newly excluded folders,
//! failures, warnings) and finish with a summary that includes how much space the
//! exclusions save. `-v` adds already-excluded folders, unreadable folders, folder sizes
//! and traces. The JSON reporter always carries the same counts, the changed folders,
//! failures and warnings.

use crate::cache::Cache;
use crate::catalog::Confidence;
use crate::config::Config;
use crate::recommend::{Applied, Report, Suggestion};
use crate::scan::{self, Event, Kind, Options, Progress, Summary};
use anyhow::Result;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// How often the live status line is redrawn.
const TICK: Duration = Duration::from_millis(80);

pub trait Reporter {
    fn event(&mut self, event: &Event);
    /// Called about every [`TICK`] while the scan runs.
    fn tick(&mut self, _progress: &Progress) {}
    fn finish(&mut self, summary: &Summary);
}

/// Runs a scan on a worker thread and feeds its events to `reporter` as they arrive.
/// Returns the summary and the cache to save for the next run (see [`scan::scan`]).
pub fn run(
    config: Config,
    opts: &Options,
    previous: &Cache,
    reporter: &mut dyn Reporter,
) -> Result<(Summary, Cache)> {
    let progress = Progress::default();
    let (tx, rx) = mpsc::channel();
    let (summary, cache) = thread::scope(|s| {
        let scan = s.spawn(|| scan::scan(config, opts, previous, tx, &progress));
        let mut last_tick = Instant::now();
        loop {
            match rx.recv_timeout(TICK) {
                Ok(event) => reporter.event(&event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if last_tick.elapsed() >= TICK {
                reporter.tick(&progress);
                last_tick = Instant::now();
            }
        }
        scan.join().expect("scan thread panicked")
    })?;
    reporter.finish(&summary);
    Ok((summary, cache))
}

/// Plain lines with absolute paths, for scripts and log files. Errors go to `err`.
/// The last line is a `key=value` summary.
pub struct LogReporter<O: Write, E: Write> {
    out: O,
    err: E,
    verbose: bool,
    dry_run: bool,
}

impl<O: Write, E: Write> LogReporter<O, E> {
    pub fn new(out: O, err: E, opts: &Options) -> Self {
        LogReporter {
            out,
            err,
            verbose: opts.verbose,
            dry_run: opts.dry_run,
        }
    }
}

impl<O: Write, E: Write> Reporter for LogReporter<O, E> {
    fn event(&mut self, event: &Event) {
        let dry = if self.dry_run { " (dry run)" } else { "" };
        let _ = match event {
            Event::Exclusion {
                path,
                rule,
                kind: Kind::New,
            } => writeln!(self.out, "✅ {} - {}{}", path.display(), rule, dry),
            Event::Exclusion {
                path,
                rule,
                kind: Kind::Recreated,
            } => writeln!(
                self.out,
                "✅ {} - {} (recreated){}",
                path.display(),
                rule,
                dry
            ),
            Event::Exclusion { path, rule, .. } if self.verbose => {
                writeln!(self.out, "🟡 {} - {}", path.display(), rule)
            }
            Event::Sized {
                path,
                bytes,
                measured,
            } if self.verbose => writeln!(
                self.out,
                "Size of {}: {} ({})",
                path.display(),
                format_bytes(*bytes),
                if *measured { "measured" } else { "cached" }
            ),
            Event::Failed { path, rule } => writeln!(
                self.err,
                "❌ {} - {} (could not be excluded)",
                path.display(),
                rule
            ),
            Event::Unreadable { path, error } if self.verbose => {
                writeln!(self.err, "⚠️  Could not read {}: {}", path.display(), error)
            }
            Event::Warning(message) => writeln!(self.err, "⚠️  {message}"),
            Event::Trace(message) => writeln!(self.out, "{message}"),
            _ => Ok(()),
        };
    }

    fn finish(&mut self, s: &Summary) {
        let _ = writeln!(
            self.out,
            "summary: folders={} exclusions={} new={} recreated={} failed={} unreadable={} \
             excluded_bytes={} new_bytes={} seconds={:.2}{}",
            s.dirs,
            s.exclusions,
            s.newly_excluded,
            s.recreated,
            s.failed,
            s.unreadable,
            s.excluded_bytes,
            s.new_bytes,
            s.elapsed.as_secs_f64(),
            if self.dry_run { " dry_run=true" } else { "" }
        );
    }
}

/// One JSON object on stdout, for scripts (`--json`). Nothing else is printed: failures
/// and warnings go into the object instead of stderr. Field names match the `key=value`
/// summary line of [`LogReporter`], so either can be parsed by the same caller.
pub struct JsonReporter<W: Write> {
    out: W,
    dry_run: bool,
    changed: Vec<Change>,
    failures: Vec<Failure>,
    warnings: Vec<String>,
}

#[derive(serde::Serialize)]
struct Change {
    path: String,
    rule: String,
    /// `"new"` or `"recreated"`.
    kind: &'static str,
}

#[derive(serde::Serialize)]
struct Failure {
    path: String,
    rule: String,
}

#[derive(serde::Serialize)]
struct JsonReport<'a> {
    folders: usize,
    exclusions: usize,
    new: usize,
    recreated: usize,
    failed: usize,
    unreadable: usize,
    excluded_bytes: u64,
    new_bytes: u64,
    measured: usize,
    previous_bytes: Option<u64>,
    seconds: f64,
    dry_run: bool,
    changed: &'a [Change],
    failures: &'a [Failure],
    warnings: &'a [String],
}

impl<W: Write> JsonReporter<W> {
    pub fn new(out: W, opts: &Options) -> Self {
        JsonReporter {
            out,
            dry_run: opts.dry_run,
            changed: Vec::new(),
            failures: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl<W: Write> Reporter for JsonReporter<W> {
    fn event(&mut self, event: &Event) {
        match event {
            Event::Exclusion { path, rule, kind } if *kind != Kind::Already => {
                self.changed.push(Change {
                    path: path.display().to_string(),
                    rule: rule.clone(),
                    kind: if *kind == Kind::New { "new" } else { "recreated" },
                })
            }
            Event::Failed { path, rule } => self.failures.push(Failure {
                path: path.display().to_string(),
                rule: rule.clone(),
            }),
            Event::Warning(message) => self.warnings.push(message.clone()),
            _ => {}
        }
    }

    fn finish(&mut self, s: &Summary) {
        let report = JsonReport {
            folders: s.dirs,
            exclusions: s.exclusions,
            new: s.newly_excluded,
            recreated: s.recreated,
            failed: s.failed,
            unreadable: s.unreadable,
            excluded_bytes: s.excluded_bytes,
            new_bytes: s.new_bytes,
            measured: s.measured,
            previous_bytes: s.previous_bytes,
            seconds: s.elapsed.as_secs_f64(),
            dry_run: self.dry_run,
            changed: &self.changed,
            failures: &self.failures,
            warnings: &self.warnings,
        };
        let _ = serde_json::to_writer(&mut self.out, &report);
        let _ = writeln!(self.out);
    }
}

/// A live status line for terminals: a spinner, the folder count and the current folder,
/// redrawn in place. Changes print above it; a short summary replaces it at the end.
pub struct CompactReporter<W: Write> {
    out: W,
    verbose: bool,
    dry_run: bool,
    color: bool,
    home: Option<PathBuf>,
    frame: usize,
    status_shown: bool,
}

impl<W: Write> CompactReporter<W> {
    pub fn new(out: W, opts: &Options, color: bool) -> Self {
        CompactReporter {
            out,
            verbose: opts.verbose,
            dry_run: opts.dry_run,
            color,
            home: dirs::home_dir(),
            frame: 0,
            status_shown: false,
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    fn short(&self, path: &Path) -> String {
        match self.home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        }
    }

    fn clear_status(&mut self) {
        if self.status_shown {
            let _ = write!(self.out, "\r\x1b[2K");
            self.status_shown = false;
        }
    }

    fn line(&mut self, text: String) {
        self.clear_status();
        let _ = writeln!(self.out, "{text}");
    }
}

impl<W: Write> Reporter for CompactReporter<W> {
    fn event(&mut self, event: &Event) {
        let text = match event {
            Event::Exclusion {
                path,
                rule,
                kind: Kind::New,
            } => {
                let dry = if self.dry_run { " (dry run)" } else { "" };
                format!(
                    "✅ {}  {}",
                    self.short(path),
                    self.dim(&format!("{rule}{dry}"))
                )
            }
            Event::Exclusion {
                path,
                rule,
                kind: Kind::Recreated,
            } => {
                let dry = if self.dry_run { " (dry run)" } else { "" };
                format!(
                    "✅ {}  {}",
                    self.short(path),
                    self.dim(&format!("{rule} · recreated{dry}"))
                )
            }
            Event::Exclusion { path, rule, .. } if self.verbose => {
                format!("🟡 {}  {}", self.short(path), self.dim(rule))
            }
            Event::Failed { path, rule } => format!(
                "❌ {}  {}  {}",
                self.short(path),
                self.dim(rule),
                self.paint("31", "could not be excluded")
            ),
            Event::Unreadable { path, error } if self.verbose => {
                let message = format!("could not read {}: {error}", self.short(path));
                format!("⚠️  {}", self.paint("33", &message))
            }
            Event::Warning(message) => format!("⚠️  {}", self.paint("33", message)),
            Event::Sized {
                path,
                bytes,
                measured,
            } if self.verbose => {
                let how = if *measured { "measured" } else { "cached" };
                self.dim(&format!(
                    "{} {}  {how}",
                    format_bytes(*bytes),
                    self.short(path)
                ))
            }
            Event::Trace(message) => self.dim(message),
            _ => return,
        };
        self.line(text);
    }

    fn tick(&mut self, progress: &Progress) {
        const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        self.frame = (self.frame + 1) % SPINNER.len();
        let to_size = progress.to_size.load(Ordering::Relaxed);
        let head = if to_size == 0 {
            format!(
                "{} {} folders · ",
                SPINNER[self.frame],
                group_digits(progress.dirs.load(Ordering::Relaxed))
            )
        } else {
            format!(
                "{} sizing {}/{} · ",
                SPINNER[self.frame],
                group_digits(progress.sized.load(Ordering::Relaxed)),
                group_digits(to_size)
            )
        };
        // Keep the line one column short of the width so it never wraps.
        let room = terminal_width().saturating_sub(head.chars().count() + 1);
        let current = truncate_left(&self.short(&progress.current()), room);
        let status = self.dim(&format!("{head}{current}"));
        self.clear_status();
        let _ = write!(self.out, "{status}");
        let _ = self.out.flush();
        self.status_shown = true;
    }

    fn finish(&mut self, s: &Summary) {
        let mut text = format!(
            "✓ Done in {:.1}s · {} · {}, {} new",
            s.elapsed.as_secs_f64(),
            counted(s.dirs, "folder", "folders"),
            counted(s.exclusions, "exclusion", "exclusions"),
            group_digits(s.newly_excluded)
        );
        if s.recreated > 0 {
            text += &format!(" ({} recreated)", group_digits(s.recreated));
        }
        if s.failed > 0 {
            text += &self.paint("31", &format!(", {} failed", s.failed));
        }
        if self.dry_run {
            text += &self.dim(" · dry run, nothing written");
        }
        self.line(text);
        self.line(format!("  {}", savings(s)));
        if s.unreadable > 0 && !self.verbose {
            let hint = format!(
                "  {} not be read; run with -v to list them",
                counted(s.unreadable, "folder could", "folders could")
            );
            self.line(self.dim(&hint));
        }
        let _ = self.out.flush();
    }
}

/// `9.3 GB excluded · +668 MB this run · +0.6 GB since last run`.
/// The last two parts are left out when there's nothing new or no previous run.
pub fn savings(s: &Summary) -> String {
    let mut text = format!("{} excluded", format_bytes(s.excluded_bytes));
    if s.newly_excluded > 0 {
        text += &format!(" · +{} this run", format_bytes(s.new_bytes));
    }
    if let Some(previous) = s.previous_bytes {
        let (sign, delta) = if s.excluded_bytes >= previous {
            ("+", s.excluded_bytes - previous)
        } else {
            ("−", previous - s.excluded_bytes)
        };
        text += &format!(" · {sign}{} since last run", format_bytes(delta));
    }
    text
}

/// What `asimeow stats` prints: savings and the largest folders, from the cache alone.
pub fn stats(cache: &Cache, now: u64, home: Option<&Path>) -> String {
    let Some(run) = &cache.last_run else {
        return "No scan recorded yet. Run asimeow first.\n".to_string();
    };
    let short = |path: &Path| match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };

    let mut out = format!(
        "{} excluded in {} · last scan {} ({})\n",
        format_bytes(run.excluded_bytes),
        counted(cache.exclusions.len(), "folder", "folders"),
        ago(now.saturating_sub(run.finished_at)),
        counted(run.dirs, "folder scanned", "folders scanned"),
    );
    let mut largest: Vec<_> = cache.exclusions.iter().collect();
    largest.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(a.0.cmp(b.0)));
    if !largest.is_empty() {
        out += "\nLargest:\n";
    }
    for (path, entry) in largest.iter().take(10) {
        out += &format!(
            "{:>9}  {}  {}\n",
            format_bytes(entry.bytes),
            short(path),
            entry.rule
        );
    }
    out
}

/// ANSI styling for plain-text reports; with color off, text passes through unchanged.
#[derive(Debug, Clone, Copy)]
pub struct Paint(pub bool);

impl Paint {
    fn code(self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    pub fn bold(self, text: &str) -> String {
        self.code("1", text)
    }
    pub fn dim(self, text: &str) -> String {
        self.code("2", text)
    }
    pub fn green(self, text: &str) -> String {
        self.code("1;32", text)
    }
    pub fn yellow(self, text: &str) -> String {
        self.code("1;33", text)
    }
    pub fn red(self, text: &str) -> String {
        self.code("1;31", text)
    }
    pub fn cyan(self, text: &str) -> String {
        self.code("36", text)
    }
}

/// What `asimeow recommend` prints: the suggestions (likely safe first, then the ones to
/// check), the tool caches, and how to apply them. `applying` leaves out the hint (the
/// `--apply` result follows instead); `verbose` lists every folder and prints the config
/// snippet for adding them by hand.
pub fn recommend_text(
    report: &Report,
    home: Option<&Path>,
    config_path: &str,
    verbose: bool,
    applying: bool,
    paint: Paint,
) -> String {
    let short = |path: &Path| match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    let mut out = paint.dim(&format!(
        "Scanned {} in {:.1}s.",
        counted(report.dirs, "folder", "folders"),
        report.elapsed.as_secs_f64()
    )) + "\n\n";

    if report.suggestions.is_empty() {
        out += "Nothing to add: your config already covers every build folder the catalog knows.\n";
    } else {
        let total: u64 = report.suggestions.iter().map(|s| s.bytes).sum();
        let folders: usize = report.suggestions.iter().map(|s| s.paths.len()).sum();
        out += &format!(
            "{} {} {}\n",
            paint.bold("Could save"),
            paint.green(&format_bytes(total)),
            paint.bold(&format!(
                "more in {}:",
                counted(folders, "folder", "folders")
            ))
        );
        for (title, confidence) in [
            (paint.green("Likely safe"), Confidence::High),
            (
                paint.yellow("Check before adding")
                    + &paint.dim(" (generic names, or tracked by git)"),
                Confidence::Check,
            ),
        ] {
            let group: Vec<&Suggestion> = report
                .suggestions
                .iter()
                .filter(|s| s.confidence == confidence)
                .collect();
            if group.is_empty() {
                continue;
            }
            out += &format!("\n  {title}\n");
            for s in group {
                let target = match &s.config_rule {
                    Some(rule) => format!("add to rule \"{rule}\""),
                    None => format!("new rule \"{}\"", s.catalog_rule),
                };
                out += &format!(
                    "  {}  {:<20} {}  {}\n",
                    paint.bold(&format!("{:>9}", format_bytes(s.bytes))),
                    s.folder,
                    paint.dim(&format!(
                        "{:>11}",
                        counted(s.paths.len(), "folder", "folders")
                    )),
                    paint.cyan(&target),
                );
                if verbose {
                    for (path, bytes) in &s.paths {
                        out += &paint.dim(&format!(
                            "  {:>9}    {}",
                            format_bytes(*bytes),
                            short(path)
                        ));
                        out += "\n";
                    }
                } else if let Some((example, _)) = s.paths.first() {
                    out += &paint.dim(&format!("             e.g. {}", short(example)));
                    out += "\n";
                }
                for path in &s.tracked {
                    out += &format!(
                        "             {}\n",
                        paint.yellow(&format!("tracked by git: {}", short(path)))
                    );
                }
            }
        }
    }

    if !report.caches.is_empty() {
        out += &format!(
            "\n{} {}\n",
            paint.bold("Tool caches"),
            paint.dim("(downloadable again; exclude one with `asimeow exclude <path>`):")
        );
        for cache in &report.caches {
            out += &format!(
                "  {}  {}{}\n",
                paint.bold(&format!("{:>9}", format_bytes(cache.bytes))),
                short(&cache.path),
                if cache.excluded {
                    paint.dim("  (already excluded)")
                } else {
                    String::new()
                }
            );
        }
    }
    if report.already_excluded > 0 {
        let n = report.already_excluded;
        out += "\n";
        out += &paint.dim(&format!(
            "{} that the catalog knows {} already excluded by other means (like cargo marking target).",
            counted(n, "more folder", "more folders"),
            if n == 1 { "is" } else { "are" }
        ));
        out += "\n";
    }

    if applying || report.suggestions.is_empty() {
        return out;
    }
    let safe: Vec<&Suggestion> = report
        .suggestions
        .iter()
        .filter(|s| s.confidence == Confidence::High)
        .collect();
    if safe.is_empty() {
        out += "\nNothing here is likely safe to apply automatically. Add the ones above by hand if they're generated";
        out += if verbose {
            ":\n"
        } else {
            " (-v prints a config snippet).\n"
        };
    } else {
        let folders: usize = safe.iter().map(|s| s.paths.len()).sum();
        let bytes: u64 = safe.iter().map(|s| s.bytes).sum();
        out += &format!(
            "\n{} Apply the likely-safe ones with {} ({}, {}).\n",
            paint.cyan("→"),
            paint.bold("asimeow recommend --apply"),
            counted(folders, "folder", "folders"),
            format_bytes(bytes)
        );
        out += &paint.dim(
            "  They're added to your config and excluded right away. Preview with --dry-run; -v prints the config snippet instead.",
        );
        out += "\n";
    }
    if verbose {
        out += &format!("\nAdd to {config_path}:\n\n{}", recommend_snippet(report));
    }
    out
}

/// What `recommend --apply` prints after the report.
pub fn applied_text(
    applied: &Applied,
    home: Option<&Path>,
    config_path: &str,
    dry_run: bool,
    paint: Paint,
) -> String {
    if applied.additions.is_empty() {
        return "\nNothing likely safe to apply.\n".to_string();
    }
    let short = |path: &Path| match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    let mut out = match (&applied.backup, dry_run) {
        (_, true) => format!(
            "\n{} would add to {config_path}:\n",
            paint.yellow("Dry run:")
        ),
        (Some(backup), false) => format!(
            "\n{} {config_path} {}\n",
            paint.green("Added to"),
            paint.dim(&format!("(previous version: {}):", short(backup)))
        ),
        (None, false) => format!("\n{} {config_path}:\n", paint.green("Added to")),
    };
    for a in &applied.additions {
        let target = if a.existing {
            format!("rule \"{}\"", a.rule)
        } else {
            format!("new rule \"{}\" ({})", a.rule, a.file_match)
        };
        out += &format!("  {}: {}\n", paint.cyan(&target), a.folders.join(", "));
    }
    let summary = format!(
        "{} {} ({}).",
        if dry_run { "Would exclude" } else { "Excluded" },
        counted(applied.marked, "folder", "folders"),
        format_bytes(applied.marked_bytes),
    );
    out += &if dry_run {
        format!(
            "{} {}\n",
            paint.bold(&summary),
            paint.dim("Nothing was written.")
        )
    } else {
        format!("{} {}\n", paint.green("✓"), paint.bold(&summary))
    };
    for path in &applied.failed {
        out += &format!(
            "{}\n",
            paint.red(&format!("❌ Could not exclude {}", short(path)))
        );
    }
    out
}

/// The config change for every likely-safe suggestion, as YAML in the config's own style.
/// Folders to check are only named in a closing comment, so the snippet stays valid YAML
/// (a new rule never ends up with an empty `exclusions`).
pub fn recommend_snippet(report: &Report) -> String {
    let mut existing: Vec<(&str, String)> = Vec::new();
    let mut new: Vec<(&str, &str, String)> = Vec::new();
    let mut to_check: Vec<String> = Vec::new();
    for s in &report.suggestions {
        if s.confidence == Confidence::Check {
            let rule = s.config_rule.as_deref().unwrap_or(&s.catalog_rule);
            to_check.push(format!("{} ({rule})", s.folder));
            continue;
        }
        let line = format!("  - {}\n", s.folder);
        match &s.config_rule {
            Some(rule) => match existing.iter_mut().find(|(r, _)| r == rule) {
                Some((_, lines)) => lines.push_str(&line),
                None => existing.push((rule, line)),
            },
            None => match new.iter_mut().find(|(r, _, _)| *r == s.catalog_rule) {
                Some((_, _, lines)) => lines.push_str(&line),
                None => new.push((&s.catalog_rule, &s.file_match, line)),
            },
        }
    }
    let mut out = String::new();
    for (rule, lines) in existing {
        out += &format!("# in rule \"{rule}\", add under exclusions:\n{lines}\n");
    }
    for (rule, file_match, lines) in new {
        out += &format!(
            "- name: {rule}\n  file_match: {}\n  exclusions:\n{lines}\n",
            yaml_scalar(file_match)
        );
    }
    if !to_check.is_empty() {
        out += &format!("# Check before adding: {}\n", to_check.join(", "));
    }
    out
}

/// Quotes a YAML value that would otherwise be read as something else (`*.csproj`).
fn yaml_scalar(value: &str) -> String {
    if value.starts_with(['*', '&', '!', '[', '{', '?', '|', '>', '%', '@', '`']) {
        format!("'{value}'")
    } else {
        value.to_string()
    }
}

/// `45` → `just now`, `7200` → `2 h ago`, `200000` → `2 days ago`.
fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => counted((seconds / 86400) as usize, "day ago", "days ago"),
    }
}

/// Decimal units, like Finder: `668000000` → `668 MB`, `9319000000` → `9.3 GB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `1, "folder", "folders"` → `1 folder`; `2000, …` → `2,000 folders`.
fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

/// `12345` → `12,345`.
pub fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Keeps the end of `text` (the interesting part of a path) within `max` characters.
pub fn truncate_left(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let tail: String = text.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}

/// True unless the `NO_COLOR` convention asks for no color.
pub fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Columns of the terminal on stdout, or 80 if unknown.
fn terminal_width() -> usize {
    use std::ffi::{c_int, c_ulong};

    #[repr(C)]
    struct Winsize {
        rows: u16,
        cols: u16,
        x_pixels: u16,
        y_pixels: u16,
    }
    extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }
    const TIOCGWINSZ: c_ulong = 0x4008_7468; // macOS

    let mut size = Winsize {
        rows: 0,
        cols: 0,
        x_pixels: 0,
        y_pixels: 0,
    };
    // SAFETY: TIOCGWINSZ writes one Winsize into the pointer we pass.
    let ok = unsafe { ioctl(1, TIOCGWINSZ, &mut size as *mut Winsize) } == 0;
    if ok && size.cols > 0 {
        size.cols as usize
    } else {
        80
    }
}
