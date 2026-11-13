//! `asimeow recommend`: finds build and dependency folders that the config doesn't
//! exclude yet, using the built-in [catalog](crate::catalog). Read-only: it marks nothing
//! and doesn't touch the config or the cache.
//!
//! In each folder, the exclusion names of every config rule that matches an entry are
//! *covered*. The names of every matching catalog rule, minus covered ones, that exist as
//! folders are *candidates*. Neither is walked into: they're generated, and walking
//! `node_modules` would find thousands of nested `package.json`.
//!
//! Package-manager caches ([`TOOL_CACHES`]) are not searched either: every cached package
//! has its own `package.json`, but its `build` folder is part of the published package,
//! not the user's project. They're reported as whole folders instead.

use crate::catalog::{CatalogRule, Confidence};
use crate::config::{expand_tilde, Config, Rule};
use crate::config_edit::{self, Addition};
use crate::rules::Matcher;
use crate::scan;
use crate::tm;
use crate::walk;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// Package-manager and toolchain caches, relative to the home folder. Everything in them
/// can be downloaded again; they don't fit the rule model (a fixed path, not a folder next
/// to a marker file), so they're listed separately.
pub const TOOL_CACHES: &[&str] = &[
    ".bun/install/cache",
    ".npm",
    ".pnpm-store",
    "Library/pnpm/store",
    ".yarn/berry/cache",
    ".cargo/registry",
    ".cargo/git",
    ".rustup",
    ".gradle/caches",
    ".m2/repository",
    "go/pkg/mod",
    ".pub-cache",
    ".nuget/packages",
    ".cache",
    "Library/Caches",
];

/// A folder the catalog says is generated, which the config doesn't exclude.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    /// The folder's name, e.g. `.next`.
    pub folder: String,
    /// The catalog rule that names it.
    pub catalog_rule: String,
    pub confidence: Confidence,
    /// The config rule with the same `file_match`, if any: the folder can be added there
    /// instead of creating a new rule.
    pub config_rule: Option<String>,
}

#[derive(Debug, Default)]
pub struct Findings {
    /// Sorted by path.
    pub candidates: Vec<Candidate>,
    /// Folders the catalog names that already carry an exclusion mark (set by hand, or by
    /// a tool such as cargo for `target`): nothing more to save.
    pub already_excluded: usize,
    /// Tool caches the walk reached (see [`TOOL_CACHES`]), sorted.
    pub caches: Vec<PathBuf>,
    pub dirs: usize,
}

/// Walks the config's roots and collects the candidates. `home` locates the tool caches.
pub fn find_candidates(
    config: &Config,
    catalog: &[CatalogRule],
    threads: usize,
    home: Option<&Path>,
) -> Result<Findings> {
    let mut roots = Vec::new();
    for root in &config.roots {
        let path = expand_tilde(&root.path)?;
        if path.is_dir() {
            roots.push(path);
        }
    }
    let finder = Finder::new(config, catalog, home);
    walk::walk(roots, threads, |dir| finder.visit(dir));

    let mut candidates = finder.candidates.into_inner().unwrap();
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    let mut caches = finder.found_caches.into_inner().unwrap();
    caches.sort();
    Ok(Findings {
        candidates,
        already_excluded: finder.already_excluded.into_inner(),
        caches,
        dirs: finder.dirs.into_inner(),
    })
}

/// Everything the walk's workers share.
struct Finder<'a> {
    config_rules: Matcher,
    catalog_rules: Matcher,
    catalog: HashMap<&'a str, &'a CatalogRule>,
    /// Lowercase `file_match` → name of the first config rule with it.
    config_by_marker: HashMap<String, String>,
    caches: HashSet<PathBuf>,
    found_caches: Mutex<Vec<PathBuf>>,
    candidates: Mutex<Vec<Candidate>>,
    already_excluded: AtomicUsize,
    dirs: AtomicUsize,
}

impl<'a> Finder<'a> {
    fn new(config: &Config, catalog: &'a [CatalogRule], home: Option<&Path>) -> Self {
        let mut config_by_marker = HashMap::new();
        for rule in &config.rules {
            config_by_marker
                .entry(rule.file_match.to_lowercase())
                .or_insert_with(|| rule.name.clone());
        }
        Finder {
            config_rules: Matcher::new(config.rules.clone(), &config.ignore),
            catalog_rules: Matcher::new(catalog.iter().map(CatalogRule::to_rule).collect(), &[]),
            catalog: catalog.iter().map(|r| (r.name.as_str(), r)).collect(),
            config_by_marker,
            caches: home
                .map(|h| TOOL_CACHES.iter().map(|c| h.join(c)).collect())
                .unwrap_or_default(),
            found_caches: Mutex::new(Vec::new()),
            candidates: Mutex::new(Vec::new()),
            already_excluded: AtomicUsize::new(0),
            dirs: AtomicUsize::new(0),
        }
    }

    /// Looks at one folder and returns the subfolders to walk next.
    fn visit(&self, dir: &Path) -> Vec<PathBuf> {
        if let Some(name) = dir.file_name() {
            if self.config_rules.is_ignored(&name.to_string_lossy()) {
                return Vec::new();
            }
        }
        if self.caches.contains(dir) {
            self.found_caches.lock().unwrap().push(dir.to_path_buf());
            return Vec::new();
        }
        self.dirs.fetch_add(1, Ordering::Relaxed);
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };

        let mut covered: HashSet<String> = HashSet::new();
        let mut catalog_hits: Vec<&Rule> = Vec::new();
        let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
        let mut stop = false;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            for rule in self.config_rules.rules_for(&name) {
                // As in a scan, "." or ".." stops the walk below this folder.
                stop |= rule.exclusions.iter().any(|e| e == "." || e == "..");
                covered.extend(rule.exclusions.iter().cloned());
            }
            catalog_hits.extend(self.catalog_rules.rules_for(&name));
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                subdirs.push((name, entry.path()));
            }
        }
        if stop {
            return Vec::new();
        }

        // Covered names, then every catalog name seen here: none of them is walked into.
        let mut generated = covered;
        for rule in catalog_hits {
            for folder in &rule.exclusions {
                if generated.insert(folder.clone()) {
                    self.consider(dir, rule, folder);
                }
            }
        }

        subdirs
            .into_iter()
            .filter(|(name, _)| !generated.contains(name))
            .map(|(_, path)| path)
            .collect()
    }

    /// Records `dir/folder` as a candidate if it exists and isn't excluded already.
    fn consider(&self, dir: &Path, rule: &Rule, folder: &str) {
        let path = dir.join(folder);
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            return;
        }
        if tm::has_exclusion_mark(&path) {
            self.already_excluded.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let catalog_rule = self.catalog[rule.name.as_str()];
        self.candidates.lock().unwrap().push(Candidate {
            path,
            folder: folder.to_string(),
            catalog_rule: rule.name.clone(),
            confidence: catalog_rule
                .confidence_of(folder)
                .unwrap_or(Confidence::Check),
            config_rule: self
                .config_by_marker
                .get(&rule.file_match.to_lowercase())
                .cloned(),
        });
    }
}

/// What git says about a candidate folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Git {
    /// Ignored by the repository: almost certainly generated.
    Ignored,
    /// Contains files the repository tracks: real source, at least partly.
    Tracked,
    /// In a repository, neither ignored nor tracked.
    Untracked,
    /// Not in a repository (or git isn't available).
    NoRepo,
}

/// Asks git about `path`: `check-ignore`, then `ls-files` if it isn't ignored.
pub fn git_status(path: &Path) -> Git {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Git::NoRepo;
    };
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(parent)
            .args(args)
            .arg("--")
            .arg(name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .and_then(|status| status.code())
    };
    match git(&["check-ignore", "-q"]) {
        Some(0) => Git::Ignored,
        Some(1) => match git(&["ls-files", "--error-unmatch"]) {
            Some(0) => Git::Tracked,
            _ => Git::Untracked,
        },
        _ => Git::NoRepo,
    }
}

/// One suggested change: a folder name of one catalog rule, with every place it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub folder: String,
    pub catalog_rule: String,
    /// The catalog rule's marker, for a new rule.
    pub file_match: String,
    /// Add to this config rule; `None` means a new rule.
    pub config_rule: Option<String>,
    /// `Check` if the catalog says so or git tracks any of the folders.
    pub confidence: Confidence,
    pub bytes: u64,
    /// Biggest first.
    pub paths: Vec<(PathBuf, u64)>,
    /// Folders that contain files git tracks.
    pub tracked: Vec<PathBuf>,
}

/// A package-manager or toolchain cache found under the roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCache {
    pub path: PathBuf,
    pub bytes: u64,
    /// Already excluded from Time Machine (its own mark, a parent, or a system rule).
    pub excluded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Biggest first.
    pub suggestions: Vec<Suggestion>,
    /// Biggest first.
    pub caches: Vec<ToolCache>,
    pub already_excluded: usize,
    pub dirs: usize,
    pub elapsed: Duration,
}

/// Finds candidates, measures them and the tool caches, asks git about each candidate and
/// groups the result into suggestions. Read-only.
pub fn recommend(
    config: &Config,
    catalog: &[CatalogRule],
    threads: usize,
    home: Option<&Path>,
) -> Result<Report> {
    let started = Instant::now();
    let findings = find_candidates(config, catalog, threads, home)?;
    let checked = parallel_map(&findings.candidates, threads, |c| {
        (scan::folder_size(&c.path), git_status(&c.path))
    });
    let mut caches = parallel_map(&findings.caches, threads, |path| ToolCache {
        path: path.clone(),
        bytes: scan::folder_size(path),
        excluded: tm::has_exclusion_mark(path) || tm::is_excluded(path),
    });
    caches.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));

    let markers: HashMap<&str, &str> = catalog
        .iter()
        .map(|r| (r.name.as_str(), r.file_match.as_str()))
        .collect();
    let mut groups: Vec<Suggestion> = Vec::new();
    for (candidate, (bytes, git)) in findings.candidates.iter().zip(checked) {
        let index = match groups
            .iter()
            .position(|g| g.folder == candidate.folder && g.catalog_rule == candidate.catalog_rule)
        {
            Some(index) => index,
            None => {
                groups.push(Suggestion {
                    folder: candidate.folder.clone(),
                    catalog_rule: candidate.catalog_rule.clone(),
                    file_match: markers
                        .get(candidate.catalog_rule.as_str())
                        .map_or_else(String::new, |m| m.to_string()),
                    config_rule: candidate.config_rule.clone(),
                    confidence: candidate.confidence,
                    bytes: 0,
                    paths: Vec::new(),
                    tracked: Vec::new(),
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[index];
        group.bytes += bytes;
        group.paths.push((candidate.path.clone(), bytes));
        if git == Git::Tracked {
            group.tracked.push(candidate.path.clone());
            group.confidence = Confidence::Check;
        }
    }
    for group in &mut groups {
        group
            .paths
            .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    }
    groups.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.folder.cmp(&b.folder)));

    Ok(Report {
        suggestions: groups,
        caches,
        already_excluded: findings.already_excluded,
        dirs: findings.dirs,
        elapsed: started.elapsed(),
    })
}

/// `f` over `items` on `threads` threads, results in the order of `items`.
fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(items.len()));
    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(index) else {
                    break;
                };
                let result = f(item);
                results.lock().unwrap().push((index, result));
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

/// The config changes for the likely-safe suggestions: folders added to existing rules,
/// and new rules. "Check before adding" suggestions are never included.
pub fn safe_additions(report: &Report) -> Vec<Addition> {
    let mut additions: Vec<Addition> = Vec::new();
    for s in report
        .suggestions
        .iter()
        .filter(|s| s.confidence == Confidence::High)
    {
        let (rule, existing) = match &s.config_rule {
            Some(rule) => (rule.clone(), true),
            None => (s.catalog_rule.clone(), false),
        };
        match additions
            .iter_mut()
            .find(|a| a.rule == rule && a.existing == existing)
        {
            Some(addition) => addition.folders.push(s.folder.clone()),
            None => additions.push(Addition {
                rule,
                file_match: s.file_match.clone(),
                folders: vec![s.folder.clone()],
                existing,
            }),
        }
    }
    additions
}

/// What `recommend --apply` did (or would do, in a dry run).
#[derive(Debug, Default)]
pub struct Applied {
    pub additions: Vec<Addition>,
    /// The copy of the old config, if the config was changed.
    pub backup: Option<PathBuf>,
    /// Folders marked (or to be marked) and their total size.
    pub marked: usize,
    pub marked_bytes: u64,
    pub failed: Vec<PathBuf>,
}

/// Applies the likely-safe suggestions: adds them to the config at `config_path` (after a
/// `.bak` copy; the edit is verified, see [`config_edit::apply`]) and marks the folders
/// found. With `dry_run`, nothing is written or marked.
pub fn apply(report: &Report, config_path: &Path, dry_run: bool) -> Result<Applied> {
    let additions = safe_additions(report);
    let safe: Vec<&Suggestion> = report
        .suggestions
        .iter()
        .filter(|s| s.confidence == Confidence::High)
        .collect();
    let mut applied = Applied {
        additions,
        ..Applied::default()
    };
    if applied.additions.is_empty() {
        return Ok(applied);
    }

    let text = fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    let edited = config_edit::apply(&text, &applied.additions)?;
    if !dry_run {
        let backup = config_path.with_extension("yaml.bak");
        fs::write(&backup, &text)
            .with_context(|| format!("Failed to write {}", backup.display()))?;
        let tmp = config_path.with_extension("yaml.tmp");
        fs::write(&tmp, &edited).with_context(|| format!("Failed to write {}", tmp.display()))?;
        fs::rename(&tmp, config_path)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;
        applied.backup = Some(backup);
    }

    for (path, bytes) in safe.iter().flat_map(|s| &s.paths) {
        if dry_run || tm::set_exclusion_mark(path, true) {
            applied.marked += 1;
            applied.marked_bytes += bytes;
        } else {
            applied.failed.push(path.clone());
        }
    }
    Ok(applied)
}
