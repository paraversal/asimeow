//! `asimeow recommend`: finds build and dependency folders that the config doesn't
//! exclude yet, using the built-in [catalog](crate::catalog). Read-only: it marks nothing
//! and doesn't touch the config or the cache.
//!
//! In each folder, the exclusion names of every config rule that matches an entry are
//! *covered*. The names of every matching catalog rule, minus covered ones, that exist as
//! folders are *candidates*. Neither is walked into: they're generated, and walking
//! `node_modules` would find thousands of nested `package.json`.

use crate::catalog::{CatalogRule, Confidence};
use crate::config::{expand_tilde, Config, Rule};
use crate::rules::Matcher;
use crate::tm;
use crate::walk;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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
    pub dirs: usize,
}

/// Walks the config's roots and collects the candidates.
pub fn find_candidates(
    config: &Config,
    catalog: &[CatalogRule],
    threads: usize,
) -> Result<Findings> {
    let mut roots = Vec::new();
    for root in &config.roots {
        let path = expand_tilde(&root.path)?;
        if path.is_dir() {
            roots.push(path);
        }
    }
    let finder = Finder::new(config, catalog);
    walk::walk(roots, threads, |dir| finder.visit(dir));

    let mut candidates = finder.candidates.into_inner().unwrap();
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Findings {
        candidates,
        already_excluded: finder.already_excluded.into_inner(),
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
    candidates: Mutex<Vec<Candidate>>,
    already_excluded: AtomicUsize,
    dirs: AtomicUsize,
}

impl<'a> Finder<'a> {
    fn new(config: &Config, catalog: &'a [CatalogRule]) -> Self {
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
