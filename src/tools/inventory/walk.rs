//! Bounded traversal and explicit ignore loading. No walker may open control
//! files implicitly: they use the same regular-file guard and byte accounting
//! as source files.
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use super::{checkpoint, read_regular, Scan};

const MAX_IGNORE_BYTES: usize = 64 * 1024;
const MAX_RULE_BYTES: usize = 4 * 1024;

struct Rules {
    parent: Option<usize>,
    ignore: Option<Gitignore>,
    gitignore: Option<Gitignore>,
    exclude: Option<Gitignore>,
    git_boundary: bool,
}

enum Pending {
    Directory(PathBuf, usize),
    Entry(PathBuf, usize),
}

pub(super) fn collect(
    root: &Path,
    scope: &Path,
    stats: &mut Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut arena = Vec::new();
    if !entry(stats, cancel, deadline) {
        return files;
    }
    let Some(mut rules) = load_rules(root, root, None, &mut arena, stats, cancel, deadline) else {
        return files;
    };
    let mut path = root.to_path_buf();
    // An exact scope checks ancestors and inherited rules directly. It never
    // enumerates unrelated siblings merely to find a known path.
    for component in scope.components() {
        if !entry(stats, cancel, deadline) {
            return files;
        }
        path.push(component);
        let metadata = match path.symlink_metadata() {
            Ok(v) => v,
            Err(_) => {
                stats.skipped += 1;
                return files;
            }
        };
        let rel = path.strip_prefix(root).expect("workspace scope");
        if metadata.file_type().is_symlink()
            || excluded(rel, metadata.is_dir(), scope)
            || ignored(&arena, rules, &path, metadata.is_dir())
        {
            return files;
        }
        if metadata.is_dir() {
            let Some(next) = load_rules(
                root,
                &path,
                Some(rules),
                &mut arena,
                stats,
                cancel,
                deadline,
            ) else {
                return files;
            };
            rules = next;
        } else if metadata.is_file() && path == root.join(scope) {
            files.push(path);
            return files;
        } else {
            stats.skipped += 1;
            return files;
        }
    }
    let mut pending = vec![Pending::Directory(path, rules)];
    while let Some(item) = pending.pop() {
        if !checkpoint(stats, cancel, deadline) {
            break;
        }
        match item {
            Pending::Directory(path, rules) => {
                if stats.entries >= stats.entry_limit {
                    stats.stopped.get_or_insert("entry_limit");
                    continue;
                }
                let mut directory = match std::fs::read_dir(&path) {
                    Ok(v) => v,
                    Err(_) => {
                        stats.skipped += 1;
                        continue;
                    }
                };
                let mut children = Vec::new();
                loop {
                    // Bound collection BEFORE sorting and filtering, including
                    // ignored entries. Only a bounded directory buffer exists.
                    if !checkpoint(stats, cancel, deadline) {
                        break;
                    }
                    if stats.entries >= stats.entry_limit {
                        stats.stopped.get_or_insert("entry_limit");
                        break;
                    }
                    let Some(child) = directory.next() else {
                        break;
                    };
                    stats.entries += 1;
                    match child {
                        Ok(child) => children.push(child.path()),
                        Err(_) => stats.skipped += 1,
                    }
                }
                children.sort();
                pending.extend(
                    children
                        .into_iter()
                        .rev()
                        .map(|path| Pending::Entry(path, rules)),
                );
            }
            Pending::Entry(path, rules) => {
                let metadata = match path.symlink_metadata() {
                    Ok(v) => v,
                    Err(_) => {
                        stats.skipped += 1;
                        continue;
                    }
                };
                let rel = path.strip_prefix(root).expect("workspace entry");
                if metadata.file_type().is_symlink()
                    || excluded(rel, metadata.is_dir(), scope)
                    || ignored(&arena, rules, &path, metadata.is_dir())
                {
                    continue;
                }
                if metadata.is_file() {
                    files.push(path);
                } else if metadata.is_dir() && stats.entries < stats.entry_limit {
                    if let Some(next) = load_rules(
                        root,
                        &path,
                        Some(rules),
                        &mut arena,
                        stats,
                        cancel,
                        deadline,
                    ) {
                        pending.push(Pending::Directory(path, next));
                    }
                } else if metadata.is_dir() {
                    stats.stopped.get_or_insert("entry_limit");
                }
            }
        }
    }
    files
}

fn entry(stats: &mut Scan, cancel: &AtomicBool, deadline: Instant) -> bool {
    if !checkpoint(stats, cancel, deadline) {
        return false;
    }
    if stats.entries >= stats.entry_limit {
        stats.stopped.get_or_insert("entry_limit");
        return false;
    }
    stats.entries += 1;
    true
}

fn excluded(path: &Path, directory: bool, scope: &Path) -> bool {
    if super::excluded(path) {
        return true;
    }
    directory
        && path.components().count() == 1
        && path.file_name().is_some_and(|name| {
            matches!(
                name.to_str(),
                Some("target" | "build" | "dist" | "coverage")
            )
        })
        && !scope.starts_with(path)
}

fn ignored(arena: &[Rules], start: usize, path: &Path, directory: bool) -> bool {
    // Family precedence outranks proximity: an ancestor .ignore whitelist can
    // override a nearer .gitignore exclusion. Within a family, nearest wins.
    for family in 0..3 {
        let mut current = Some(start);
        while let Some(index) = current {
            let rules = &arena[index];
            let matcher = match family {
                0 => &rules.ignore,
                1 => &rules.gitignore,
                _ => &rules.exclude,
            };
            if let Some(matcher) = matcher {
                let matched = matcher.matched(path, directory);
                if !matched.is_none() {
                    return matched.is_ignore();
                }
            }
            if family != 0 && rules.git_boundary {
                break;
            }
            current = rules.parent;
        }
    }
    false
}

fn load_rules(
    root: &Path,
    directory: &Path,
    parent: Option<usize>,
    arena: &mut Vec<Rules>,
    stats: &mut Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Option<usize> {
    let result = (|| -> anyhow::Result<Rules> {
        let ignore = load(
            root,
            directory,
            &directory.join(".ignore"),
            stats,
            cancel,
            deadline,
        )?;
        let gitignore = load(
            root,
            directory,
            &directory.join(".gitignore"),
            stats,
            cancel,
            deadline,
        )?;
        let marker =
            super::guarded_metadata(root, &directory.join(".git"), stats, cancel, deadline)?;
        let git_boundary = marker.is_some();
        // A worktree .git file is a boundary, never a request to read its
        // outside-workspace pointer. Only local directory excludes are loaded.
        let exclude = if marker.is_some_and(|m| m.is_dir()) {
            load(
                root,
                directory,
                &directory.join(".git/info/exclude"),
                stats,
                cancel,
                deadline,
            )?
        } else {
            None
        };
        Ok(Rules {
            parent,
            ignore,
            gitignore,
            exclude,
            git_boundary,
        })
    })();
    match result {
        Ok(rules) => {
            let index = arena.len();
            arena.push(rules);
            Some(index)
        }
        Err(_) => {
            stats.skipped += 1;
            stats.ignore_errors += 1;
            None // Fail closed; unknown rules never authorize the subtree.
        }
    }
}

fn load(
    root: &Path,
    directory: &Path,
    path: &Path,
    stats: &mut Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> anyhow::Result<Option<Gitignore>> {
    if super::guarded_metadata(root, path, stats, cancel, deadline)?.is_none() {
        return Ok(None);
    }
    let bytes = read_regular(root, path, MAX_IGNORE_BYTES, stats, cancel, deadline)?;
    anyhow::ensure!(!bytes.contains(&0), "invalid ignore bytes");
    let text = std::str::from_utf8(&bytes)?;
    let mut builder = GitignoreBuilder::new(directory);
    for (index, line) in text.lines().enumerate() {
        anyhow::ensure!(checkpoint(stats, cancel, deadline), "scan stopped");
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        anyhow::ensure!(line.len() <= MAX_RULE_BYTES, "oversize ignore rule");
        builder.add_line(Some(path.to_path_buf()), line)?;
    }
    anyhow::ensure!(checkpoint(stats, cancel, deadline), "scan stopped");
    Ok(Some(builder.build()?))
}
