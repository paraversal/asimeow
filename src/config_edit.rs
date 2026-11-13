//! Adds folders and rules to a config file by editing its text, so comments, quoting and
//! layout stay as they are (rewriting through serde would drop comments).
//!
//! Supported layouts: a top-level `rules:` block list, with each rule's `exclusions` as a
//! block list (`- obj` lines) or a one-line flow list (`[obj, bin]`). Anything else is an
//! error rather than a guess. Every edit is verified: the result is parsed again and must
//! equal the old config plus exactly the additions.

use crate::config::{Config, Rule};
use anyhow::{anyhow, bail, Context, Result};

/// Folders to add to a rule. `existing` rules are found by name; others are appended as
/// new rules with `file_match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addition {
    pub rule: String,
    pub file_match: String,
    pub folders: Vec<String>,
    pub existing: bool,
}

/// Returns `text` with the additions applied, verified by parsing.
pub fn apply(text: &str, additions: &[Addition]) -> Result<String> {
    let before: Config = serde_yaml::from_str(text).context("The config doesn't parse")?;
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    // Existing rules first: inserting lines inside the block doesn't move its start.
    for addition in additions.iter().filter(|a| a.existing) {
        add_to_rule(&mut lines, addition)?;
    }
    let new_rules: Vec<&Addition> = additions.iter().filter(|a| !a.existing).collect();
    if !new_rules.is_empty() {
        append_rules(&mut lines, &new_rules)?;
    }

    let mut edited = lines.join("\n");
    if text.ends_with('\n') {
        edited.push('\n');
    }
    verify(&before, &edited, additions)?;
    Ok(edited)
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty() || t.starts_with('#')
}

/// Removes quotes and a trailing comment from a scalar value.
fn unquote(value: &str) -> String {
    let value = value.split(" #").next().unwrap_or(value).trim();
    value.trim_matches(|c| c == '"' || c == '\'').to_string()
}

/// Lines `start..end` of the top-level `rules:` list (`start` is the first line after
/// `rules:`), and the indent of its `- ` items.
fn rules_block(lines: &[String]) -> Result<(usize, usize, usize)> {
    let header = lines
        .iter()
        .position(|l| {
            let t = l.split(" #").next().unwrap_or(l).trim_end();
            t == "rules:"
        })
        .ok_or_else(|| anyhow!("No `rules:` block list found in the config"))?;
    let start = header + 1;
    let end = (start..lines.len())
        .find(|&i| {
            let l = &lines[i];
            !is_blank_or_comment(l) && indent_of(l) == 0 && !l.starts_with('-')
        })
        .unwrap_or(lines.len());
    let item_indent = lines[start..end]
        .iter()
        .find(|l| l.trim_start().starts_with("- "))
        .map(|l| indent_of(l))
        .ok_or_else(|| anyhow!("The `rules:` list is empty or not a block list"))?;
    Ok((start, end, item_indent))
}

/// Line ranges of each rule item in the rules block.
fn rule_items(
    lines: &[String],
    start: usize,
    end: usize,
    item_indent: usize,
) -> Vec<(usize, usize)> {
    let starts: Vec<usize> = (start..end)
        .filter(|&i| indent_of(&lines[i]) == item_indent && lines[i].trim_start().starts_with("- "))
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| (s, starts.get(n + 1).copied().unwrap_or(end)))
        .collect()
}

/// The value of `key:` on a line of a rule item (the item's first line has a `- ` prefix).
fn key_value(line: &str, key: &str) -> Option<String> {
    let t = line.trim_start().trim_start_matches("- ").trim_start();
    t.strip_prefix(key)
        .and_then(|rest| rest.strip_prefix(':'))
        .map(|v| v.trim().to_string())
}

fn add_to_rule(lines: &mut Vec<String>, addition: &Addition) -> Result<()> {
    let (start, end, item_indent) = rules_block(lines)?;
    let (item_start, item_end) = rule_items(lines, start, end, item_indent)
        .into_iter()
        .find(|&(s, e)| {
            (s..e)
                .any(|i| key_value(&lines[i], "name").is_some_and(|v| unquote(&v) == addition.rule))
        })
        .ok_or_else(|| anyhow!("Rule \"{}\" not found in the config", addition.rule))?;
    let key_line = (item_start..item_end)
        .find(|&i| key_value(&lines[i], "exclusions").is_some())
        .ok_or_else(|| anyhow!("Rule \"{}\" has no `exclusions`", addition.rule))?;
    let value = key_value(&lines[key_line], "exclusions").unwrap_or_default();
    let value = value
        .split(" #")
        .next()
        .unwrap_or(&value)
        .trim()
        .to_string();

    if value.starts_with('[') {
        // Flow list on one line: insert before the closing bracket.
        let close = lines[key_line]
            .rfind(']')
            .ok_or_else(|| anyhow!("Rule \"{}\": `exclusions` spans lines", addition.rule))?;
        let inner = value
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or("")
            .trim();
        let quote = if inner.starts_with('"') { "\"" } else { "" };
        let added: Vec<String> = addition
            .folders
            .iter()
            .map(|f| format!("{quote}{f}{quote}"))
            .collect();
        let separator = if inner.is_empty() { "" } else { ", " };
        lines[key_line].insert_str(close, &format!("{separator}{}", added.join(", ")));
        return Ok(());
    }
    if !value.is_empty() {
        bail!("Rule \"{}\": unsupported `exclusions` value", addition.rule);
    }

    // Block list: the items follow the key at the key's indent or deeper.
    let key_indent = indent_of(&lines[key_line])
        + if lines[key_line].trim_start().starts_with("- ") {
            2
        } else {
            0
        };
    let mut last = key_line;
    let mut list_indent = None;
    for (i, line) in lines.iter().enumerate().take(item_end).skip(key_line + 1) {
        if is_blank_or_comment(line) {
            continue;
        }
        let indent = indent_of(line);
        if line.trim_start().starts_with("- ") && indent >= key_indent {
            list_indent.get_or_insert(indent);
            last = i;
        } else if indent <= key_indent {
            break;
        }
    }
    let list_indent =
        list_indent.ok_or_else(|| anyhow!("Rule \"{}\": `exclusions` is empty", addition.rule))?;
    let pad = " ".repeat(list_indent);
    for (n, folder) in addition.folders.iter().enumerate() {
        lines.insert(last + 1 + n, format!("{pad}- {}", scalar(folder)));
    }
    Ok(())
}

fn append_rules(lines: &mut Vec<String>, additions: &[&Addition]) -> Result<()> {
    let (start, end, item_indent) = rules_block(lines)?;
    // Copy the existing layout: where keys and list items sit relative to `- `.
    let key_indent = item_indent + 2;
    let list_indent = (start..end)
        .find(|&i| key_value(&lines[i], "exclusions").is_some_and(|v| v.is_empty()))
        .and_then(|i| lines.get(i + 1))
        .filter(|l| l.trim_start().starts_with("- "))
        .map(|l| indent_of(l))
        .unwrap_or(key_indent);
    let mut block = Vec::new();
    for addition in additions {
        block.push(format!(
            "{}- name: {}",
            " ".repeat(item_indent),
            addition.rule
        ));
        block.push(format!(
            "{}file_match: {}",
            " ".repeat(key_indent),
            scalar(&addition.file_match)
        ));
        block.push(format!("{}exclusions:", " ".repeat(key_indent)));
        for folder in &addition.folders {
            block.push(format!("{}- {}", " ".repeat(list_indent), scalar(folder)));
        }
    }
    // After the last non-blank line of the block, before any trailing blank lines.
    let mut at = end;
    while at > start && lines[at - 1].trim().is_empty() {
        at -= 1;
    }
    lines.splice(at..at, block);
    Ok(())
}

/// Quotes a value that YAML would otherwise read as something else.
fn scalar(value: &str) -> String {
    let special = value.starts_with([
        '*', '&', '!', '[', '{', '?', '|', '>', '%', '@', '`', '#', '\'', '"',
    ]) || value.contains(": ")
        || value.contains(" #");
    if special {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        value.to_string()
    }
}

/// The edited text must parse to the old config plus exactly the additions.
fn verify(before: &Config, edited: &str, additions: &[Addition]) -> Result<()> {
    let after: Config = serde_yaml::from_str(edited)
        .context("The edited config doesn't parse; nothing was written")?;
    let mut expected = before.clone();
    for addition in additions {
        if addition.existing {
            let rule = expected
                .rules
                .iter_mut()
                .find(|r| r.name == addition.rule)
                .ok_or_else(|| anyhow!("Rule \"{}\" not found", addition.rule))?;
            rule.exclusions.extend(addition.folders.iter().cloned());
        } else {
            expected.rules.push(Rule {
                name: addition.rule.clone(),
                file_match: addition.file_match.clone(),
                exclusions: addition.folders.clone(),
            });
        }
    }
    if after != expected {
        bail!("Editing the config would change more than intended; nothing was written");
    }
    Ok(())
}
