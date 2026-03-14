//! # difftastic-nvim
//!
//! A Neovim plugin for displaying difftastic diffs in a side-by-side viewer.
//!
//! This crate provides Lua bindings for parsing [difftastic](https://difftastic.wilfred.me.uk/)
//! JSON output and processing it into a display-ready format. It supports both
//! [jj](https://github.com/martinvonz/jj) and [git](https://git-scm.com/) version control systems.
//!
//! ## Architecture
//!
//! The crate is organized into three modules:
//!
//! - `difftastic` - Types and parsing for difftastic's JSON output format
//! - `processor` - Transforms parsed data into aligned side-by-side display rows
//! - `lib` (this module) - Lua bindings and VCS integration
//!
//! ## Usage from Lua
//!
//! ```lua
//! local difft = require("difftastic_nvim")
//!
//! -- Get diff for a jj revision
//! local result = difft.run_diff("@", "jj")
//!
//! -- Get diff for a git commit
//! local result = difft.run_diff("HEAD", "git")
//!
//! -- Get diff for a git commit range
//! local result = difft.run_diff("main..feature", "git")
//! ```
//!
//! ## Environment Variables
//!
//! This crate sets the following environment variables when invoking difftastic:
//!
//! - `DFT_DISPLAY=json` - Enables JSON output mode
//! - `DFT_UNSTABLE=yes` - Enables unstable features (required for JSON output)

use mlua::prelude::*;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

mod difftastic;
mod processor;

/// Splits file content into individual lines, or empty vector if `None`.
#[inline]
fn into_lines(content: Option<String>) -> Vec<String> {
    content
        .map(|c| c.lines().map(String::from).collect())
        .unwrap_or_default()
}

/// Fetches file content from jj at a specific revision via `jj file show`.
/// Returns `None` if the command fails or the file doesn't exist.
fn jj_file_content(revset: &str, path: &Path) -> Option<String> {
    Command::new("jj")
        .args(["file", "show", "-r", revset])
        .arg(path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fetches file content from git at a specific commit via `git show`.
/// Returns `None` if the command fails or the file doesn't exist.
fn git_file_content(commit: &str, path: &Path) -> Option<String> {
    Command::new("git")
        .arg("show")
        .arg(format!("{commit}:{}", path.display()))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fetches file content from git index (staged version).
/// Returns `None` if the command fails or the file doesn't exist in the index.
fn git_index_content(path: &Path) -> Option<String> {
    Command::new("git")
        .arg("show")
        .arg(format!(":{}", path.display()))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Gets the git repository root directory.
fn git_root() -> Option<PathBuf> {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

/// Gets the jj repository root directory.
fn jj_root() -> Option<PathBuf> {
    Command::new("jj")
        .args(["root"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

/// Stats for a single file: (additions, deletions).
type FileStats = HashMap<PathBuf, (u32, u32)>;

/// Gets diff stats from git using `--numstat`.
/// Output format: "additions\tdeletions\tpath"
///
/// Pass additional arguments to customize the diff:
/// - `&["HEAD^..HEAD"]` for a commit range
/// - `&[]` for working tree vs index
/// - `&["--cached"]` for index vs HEAD
fn git_diff_stats(extra_args: &[&str]) -> FileStats {
    let mut args = vec!["diff", "--numstat"];
    args.extend(extra_args);

    let output = Command::new("git").args(&args).output().ok();

    let Some(output) = output.filter(|o| o.status.success()) else {
        return HashMap::new();
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let add = parts.next()?.parse().ok()?;
            let del = parts.next()?.parse().ok()?;
            let path = parts.next()?;
            Some((PathBuf::from(path), (add, del)))
        })
        .collect()
}

/// Gets diff stats for jj working copy vs current commit.
fn jj_diff_stats_uncommitted() -> FileStats {
    // jj diff without -r compares working copy to the current commit
    let output = Command::new("jj").args(["diff", "--stat"]).output().ok();

    // jj --stat output is different, so we just return empty for now
    // The diff will still work, just without inline stats
    let _ = output;
    HashMap::new()
}

/// Translates a jj revset to a git commit hash.
/// Uses `jj log -r <revset> --no-graph -T 'commit_id'`.
fn jj_to_git_commit(revset: &str) -> Option<String> {
    let output = Command::new("jj")
        .args(["log", "-r", revset, "--no-graph", "-T", "commit_id"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let commits: Vec<_> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    if commits.len() != 1 {
        return None;
    }

    let commit = commits[0].to_string();
    // Valid git commit hash is 40 hex characters
    (commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit())).then_some(commit)
}

/// Parses a jj range of the form `A..B` into `(A, B)`.
/// Returns `None` for non-range revsets.
#[inline]
fn parse_jj_range(revset: &str) -> Option<(String, String)> {
    let (old, new) = revset.split_once("..")?;
    if old.is_empty() || new.is_empty() {
        return None;
    }
    Some((old.to_string(), new.to_string()))
}

/// Gets diff stats from jj by translating revsets to git commits.
/// For colocated repos, uses `git diff --numstat` for accurate stats.
fn jj_diff_stats(revset: &str) -> FileStats {
    if let Some((old_rev, new_rev)) = parse_jj_range(revset) {
        let old_commit = jj_to_git_commit(&old_rev);
        let new_commit = jj_to_git_commit(&new_rev);
        return match (old_commit, new_commit) {
            (Some(old), Some(new)) => git_diff_stats(&[&format!("{old}..{new}")]),
            _ => HashMap::new(),
        };
    }

    let old_commit = jj_to_git_commit(&format!("roots({revset})-"));
    let new_commit = jj_to_git_commit(&format!("heads({revset})"));

    match (old_commit, new_commit) {
        (Some(old), Some(new)) => git_diff_stats(&[&format!("{old}..{new}")]),
        (None, Some(new)) => git_diff_stats(&[&format!("{new}^..{new}")]),
        _ => HashMap::new(),
    }
}

/// Runs difftastic via jj and parses the JSON output.
/// Executes `jj diff -r <revset> --tool difft` with JSON output mode enabled.
fn run_jj_diff(revset: &str) -> Result<Vec<difftastic::DifftFile>, String> {
    let output = Command::new("jj")
        .args(["diff", "-r", revset, "--tool", "difft"])
        .env("DFT_DISPLAY", "json")
        .env("DFT_UNSTABLE", "yes")
        .output()
        .map_err(|e| format!("Failed to run jj: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("jj command failed: {stderr}"));
    }

    difftastic::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|e| format!("Failed to parse difftastic JSON: {e}"))
}

/// Runs difftastic via jj for working copy vs current commit.
/// Executes `jj diff` with no revision argument.
fn run_jj_diff_uncommitted() -> Result<Vec<difftastic::DifftFile>, String> {
    let output = Command::new("jj")
        .args(["diff", "--tool", "difft"])
        .env("DFT_DISPLAY", "json")
        .env("DFT_UNSTABLE", "yes")
        .output()
        .map_err(|e| format!("Failed to run jj: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("jj command failed: {stderr}"));
    }

    difftastic::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|e| format!("Failed to parse difftastic JSON: {e}"))
}

/// Runs difftastic via git and parses the JSON output.
/// Executes `git diff` with difftastic as the external diff tool.
///
/// Pass additional arguments to customize the diff:
/// - `&["HEAD^..HEAD"]` for a commit range
/// - `&[]` for working tree vs index
/// - `&["--cached"]` for index vs HEAD
fn run_git_diff(extra_args: &[&str]) -> Result<Vec<difftastic::DifftFile>, String> {
    let mut args = vec!["-c", "diff.external=difft", "diff"];
    args.extend(extra_args);

    let output = Command::new("git")
        .args(&args)
        .env("DFT_DISPLAY", "json")
        .env("DFT_UNSTABLE", "yes")
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git command failed: {stderr}"));
    }

    difftastic::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|e| format!("Failed to parse difftastic JSON: {e}"))
}

/// Entry from `git diff --name-status` output.
struct ChangedEntry {
    status: String,
    old_path: PathBuf,
    new_path: PathBuf,
}

/// Parses `git diff --name-status` output into structured entries.
fn parse_name_status_entries(output: &str) -> Vec<ChangedEntry> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let status = parts.next()?.trim().to_string();
            let first_path = PathBuf::from(parts.next()?.trim());
            let second_path = parts.next().map(|p| PathBuf::from(p.trim()));

            // For renames (R100) and copies (C100), there are two paths
            if status.starts_with('R') || status.starts_with('C') {
                Some(ChangedEntry {
                    status,
                    old_path: first_path,
                    new_path: second_path?,
                })
            } else {
                Some(ChangedEntry {
                    status,
                    old_path: first_path.clone(),
                    new_path: first_path,
                })
            }
        })
        .collect()
}

/// Gets the full list of changed files from `git diff --name-status`.
fn git_changed_files(extra_args: &[&str]) -> Result<Vec<ChangedEntry>, String> {
    let mut args = vec!["diff", "--name-status"];
    args.extend(extra_args);

    let output = Command::new("git")
        .args(&args)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git name-status failed: {stderr}"));
    }

    Ok(parse_name_status_entries(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Alternative to `run_git_diff` that calls difft directly on each file pair in parallel.
///
/// Instead of `git -c diff.external=difft diff` (which spawns difft sequentially per file),
/// this fetches the file list via `git diff --name-status`, then for each file:
/// 1. Fetches old/new content via `git show`
/// 2. Writes temp files (preserving extension for language detection)
/// 3. Runs `difft old new` in parallel via rayon
/// 4. Parses each JSON result and fixes the path
/// Strict error handling: every file must produce exactly one result, or the entire
/// operation fails. No silent fallbacks, no partial results. This is a code review tool
/// — showing incomplete diffs is worse than showing an error.
fn run_git_diff_parallel(
    extra_args: &[&str],
    old_ref: &str,
    new_ref: &str,
) -> Result<Vec<difftastic::DifftFile>, String> {
    let entries = git_changed_files(extra_args)?;
    let expected_count = entries.len();

    // Save expected file paths and statuses for post-diff verification.
    // This is our source of truth from git — after diffing, we verify
    // the output matches what git told us to expect.
    let expected_files: Vec<(PathBuf, String)> = entries
        .iter()
        .map(|e| (e.new_path.clone(), e.status.clone()))
        .collect();

    let tmp_base = std::env::temp_dir().join(format!("difft_par_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_base)
        .map_err(|e| format!("Failed to create temp dir: {e}"))?;

    let results: Vec<Result<difftastic::DifftFile, String>> = entries
        .into_par_iter()
        .enumerate()
        .map(|(i, entry)| {
            let path_display = entry.new_path.display().to_string();

            let slot = tmp_base.join(i.to_string());
            std::fs::create_dir_all(&slot)
                .map_err(|e| format!("{path_display}: temp dir: {e}"))?;

            // Use subdirectories with the original filename so difft's language
            // detection works correctly (e.g. "CMakeLists.txt" must keep its name).
            let old_dir = slot.join("old");
            let new_dir = slot.join("new");
            std::fs::create_dir_all(&old_dir)
                .map_err(|e| format!("{path_display}: old dir: {e}"))?;
            std::fs::create_dir_all(&new_dir)
                .map_err(|e| format!("{path_display}: new dir: {e}"))?;

            let old_filename = entry
                .old_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let new_filename = entry
                .new_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let old_tmp = old_dir.join(if old_filename.is_empty() {
                "file"
            } else {
                &old_filename
            });
            let new_tmp = new_dir.join(if new_filename.is_empty() {
                "file"
            } else {
                &new_filename
            });

            // Fetch content — errors are fatal, not silent.
            // For new files (A), old is empty. For deleted files (D), new is empty.
            // For everything else, content MUST be fetchable.
            let old_content = if entry.status.starts_with('A') {
                String::new()
            } else {
                git_file_content(old_ref, &entry.old_path).ok_or_else(|| {
                    format!(
                        "{path_display}: failed to fetch old content from {old_ref}:{}",
                        entry.old_path.display()
                    )
                })?
            };

            let new_content = if entry.status.starts_with('D') {
                String::new()
            } else {
                git_file_content(new_ref, &entry.new_path).ok_or_else(|| {
                    format!(
                        "{path_display}: failed to fetch new content from {new_ref}:{}",
                        entry.new_path.display()
                    )
                })?
            };

            std::fs::write(&old_tmp, &old_content)
                .map_err(|e| format!("{path_display}: write old: {e}"))?;
            std::fs::write(&new_tmp, &new_content)
                .map_err(|e| format!("{path_display}: write new: {e}"))?;

            let output = Command::new("difft")
                .arg(&old_tmp)
                .arg(&new_tmp)
                .env("DFT_DISPLAY", "json")
                .env("DFT_UNSTABLE", "yes")
                .output()
                .map_err(|e| format!("{path_display}: difft failed to run: {e}"))?;

            // difft exit code 0 = no changes, 1 = changes found, other = error.
            // Exit codes 0 and 1 are both valid.
            let exit_code = output.status.code().unwrap_or(-1);
            if exit_code != 0 && exit_code != 1 {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!(
                    "{path_display}: difft exited with code {exit_code}: {stderr}"
                ));
            }

            let json = String::from_utf8_lossy(&output.stdout);
            if json.trim().is_empty() {
                // difft found no syntactic changes — return an "unchanged" entry
                // so every input file has exactly one output.
                let lang = language_from_ext(&entry.new_path);
                return Ok(difftastic::DifftFile {
                    path: entry.new_path,
                    language: lang,
                    status: difftastic::Status::Unchanged,
                    aligned_lines: vec![],
                    chunks: vec![],
                });
            }

            let mut parsed = difftastic::parse(&json)
                .map_err(|e| format!("{path_display}: JSON parse error: {e}"))?;

            // difft must produce exactly one file entry per invocation
            if parsed.len() != 1 {
                return Err(format!(
                    "{path_display}: expected 1 file from difft, got {}",
                    parsed.len()
                ));
            }

            let mut file = parsed.remove(0);
            file.path = entry.new_path;
            Ok(file)
        })
        .collect();

    let _ = std::fs::remove_dir_all(&tmp_base);

    // Collect results — any single file failure fails the whole batch
    let mut all_files = Vec::with_capacity(expected_count);
    for result in results {
        all_files.push(result?);
    }

    // === Post-diff verification ===
    // Cross-check output against what git reported. These are cheap O(n)
    // checks that catch content-fetch mixups or dropped files.

    // 1. Count check: output must exactly match input
    if all_files.len() != expected_count {
        return Err(format!(
            "Integrity: git reported {} files but diff produced {}",
            expected_count,
            all_files.len()
        ));
    }

    // 2. Path check: every file git reported must appear in output
    let output_paths: HashSet<&Path> = all_files.iter().map(|f| f.path.as_path()).collect();
    for (path, _) in &expected_files {
        if !output_paths.contains(path.as_path()) {
            return Err(format!(
                "Integrity: file {} reported by git but missing from diff output",
                path.display()
            ));
        }
    }

    // 3. Status contradiction check: Added files can't become Deleted,
    //    Deleted files can't become Created. These would mean content
    //    was fetched for the wrong side.
    for (expected_path, git_status) in &expected_files {
        if let Some(file) = all_files.iter().find(|f| f.path == *expected_path) {
            if git_status.starts_with('A') && file.status == difftastic::Status::Deleted {
                return Err(format!(
                    "Integrity: git says {} is Added but difft says Deleted",
                    expected_path.display()
                ));
            }
            if git_status.starts_with('D') && file.status == difftastic::Status::Created {
                return Err(format!(
                    "Integrity: git says {} is Deleted but difft says Created",
                    expected_path.display()
                ));
            }
        }
    }

    Ok(all_files)
}

/// Lua-callable: compare current (sequential) vs parallel diff approach.
///
/// Returns a table with timing data and per-file comparison details.
/// Call from Neovim: `:lua print(vim.inspect(require("difftastic_nvim").compare_diff_methods("HEAD~5..HEAD", "git")))`
/// Deep structural comparison of two DifftFiles, ignoring the `path` field.
///
/// Returns `None` if structurally identical, or `Some(description)` with the
/// first difference found. Checks every field that affects what the user sees:
/// status, language, every aligned_line pair, every chunk, every DiffLine,
/// every Side (line_number + changes), every Change (start, end, content, highlight).
fn deep_compare_files(
    c: &difftastic::DifftFile,
    p: &difftastic::DifftFile,
) -> Option<String> {
    if c.status != p.status {
        return Some(format!("status: {:?} vs {:?}", c.status, p.status));
    }
    if c.language != p.language {
        return Some(format!("language: {:?} vs {:?}", c.language, p.language));
    }

    // Aligned lines — exact pair-by-pair comparison
    if c.aligned_lines.len() != p.aligned_lines.len() {
        return Some(format!(
            "aligned_lines count: {} vs {}",
            c.aligned_lines.len(),
            p.aligned_lines.len()
        ));
    }
    for (i, (ca, pa)) in c.aligned_lines.iter().zip(&p.aligned_lines).enumerate() {
        if ca != pa {
            return Some(format!(
                "aligned_lines[{}]: ({:?},{:?}) vs ({:?},{:?})",
                i, ca.0, ca.1, pa.0, pa.1
            ));
        }
    }

    // Chunks — deep comparison of every DiffLine, Side, and Change
    if c.chunks.len() != p.chunks.len() {
        return Some(format!(
            "chunk count: {} vs {}",
            c.chunks.len(),
            p.chunks.len()
        ));
    }
    for (ci, (cc, pc)) in c.chunks.iter().zip(&p.chunks).enumerate() {
        if cc.len() != pc.len() {
            return Some(format!(
                "chunk[{}] line count: {} vs {}",
                ci,
                cc.len(),
                pc.len()
            ));
        }
        for (li, (cl, pl)) in cc.iter().zip(pc).enumerate() {
            // Compare lhs
            match (&cl.lhs, &pl.lhs) {
                (None, None) => {}
                (Some(_), None) => {
                    return Some(format!(
                        "chunk[{}].line[{}].lhs: present vs absent",
                        ci, li
                    ));
                }
                (None, Some(_)) => {
                    return Some(format!(
                        "chunk[{}].line[{}].lhs: absent vs present",
                        ci, li
                    ));
                }
                (Some(cs), Some(ps)) => {
                    if let Some(diff) = deep_compare_sides(cs, ps, ci, li, "lhs") {
                        return Some(diff);
                    }
                }
            }
            // Compare rhs
            match (&cl.rhs, &pl.rhs) {
                (None, None) => {}
                (Some(_), None) => {
                    return Some(format!(
                        "chunk[{}].line[{}].rhs: present vs absent",
                        ci, li
                    ));
                }
                (None, Some(_)) => {
                    return Some(format!(
                        "chunk[{}].line[{}].rhs: absent vs present",
                        ci, li
                    ));
                }
                (Some(cs), Some(ps)) => {
                    if let Some(diff) = deep_compare_sides(cs, ps, ci, li, "rhs") {
                        return Some(diff);
                    }
                }
            }
        }
    }

    None
}

/// Compare two Sides within a DiffLine, returning the first difference found.
fn deep_compare_sides(
    cs: &difftastic::Side,
    ps: &difftastic::Side,
    chunk_idx: usize,
    line_idx: usize,
    side_name: &str,
) -> Option<String> {
    if cs.line_number != ps.line_number {
        return Some(format!(
            "chunk[{}].line[{}].{}.line_number: {} vs {}",
            chunk_idx, line_idx, side_name, cs.line_number, ps.line_number
        ));
    }
    if cs.changes.len() != ps.changes.len() {
        return Some(format!(
            "chunk[{}].line[{}].{}.changes count: {} vs {}",
            chunk_idx,
            line_idx,
            side_name,
            cs.changes.len(),
            ps.changes.len()
        ));
    }
    for (i, (cc, pc)) in cs.changes.iter().zip(&ps.changes).enumerate() {
        if cc.start != pc.start {
            return Some(format!(
                "chunk[{}].line[{}].{}.changes[{}].start: {} vs {}",
                chunk_idx, line_idx, side_name, i, cc.start, pc.start
            ));
        }
        if cc.end != pc.end {
            return Some(format!(
                "chunk[{}].line[{}].{}.changes[{}].end: {} vs {}",
                chunk_idx, line_idx, side_name, i, cc.end, pc.end
            ));
        }
        if cc.content != pc.content {
            return Some(format!(
                "chunk[{}].line[{}].{}.changes[{}].content: {:?} vs {:?}",
                chunk_idx, line_idx, side_name, i, cc.content, pc.content
            ));
        }
        if cc.highlight != pc.highlight {
            return Some(format!(
                "chunk[{}].line[{}].{}.changes[{}].highlight: {:?} vs {:?}",
                chunk_idx, line_idx, side_name, i, cc.highlight, pc.highlight
            ));
        }
    }
    None
}

/// Verify parallel diff matches sequential for a given range.
/// Runs both approaches once, does deep structural comparison on every field.
/// Returns a Lua table: { passed = bool, files = N, message = "..." }
fn verify_diff(lua: &Lua, (range, vcs): (String, String)) -> LuaResult<LuaTable> {
    if vcs != "git" {
        return Err(LuaError::RuntimeError("verify only supports git".into()));
    }

    let (old_ref, new_ref) = parse_git_range(&range);
    let range_arg = format!("{old_ref}..{new_ref}");
    let extra_args = vec![range_arg.as_str()];

    let current = run_git_diff(&extra_args).map_err(LuaError::RuntimeError)?;
    let parallel = run_git_diff_parallel(&extra_args, &old_ref, &new_ref)
        .map_err(LuaError::RuntimeError)?;

    let normalize_path = |p: &Path| -> PathBuf {
        let (_, new_path) = split_display_path(p);
        new_path
    };

    let current_map: HashMap<PathBuf, &difftastic::DifftFile> = current
        .iter()
        .map(|f| (normalize_path(&f.path), f))
        .collect();

    let parallel_map: HashMap<PathBuf, &difftastic::DifftFile> = parallel
        .iter()
        .map(|f| (normalize_path(&f.path), f))
        .collect();

    let result = lua.create_table()?;

    // Check file count
    if current.len() != parallel.len() {
        result.set("passed", false)?;
        result.set("files", current.len())?;
        result.set(
            "message",
            format!(
                "File count mismatch: sequential={} parallel={}",
                current.len(),
                parallel.len()
            ),
        )?;
        return Ok(result);
    }

    // Deep compare each file
    let mut errors: Vec<String> = Vec::new();
    for (path, c_file) in &current_map {
        match parallel_map.get(path) {
            None => errors.push(format!("{}: missing from parallel", path.display())),
            Some(p_file) => {
                if let Some(diff) = deep_compare_files(c_file, p_file) {
                    errors.push(format!("{}: {}", path.display(), diff));
                }
            }
        }
    }
    for path in parallel_map.keys() {
        if !current_map.contains_key(path) {
            errors.push(format!("{}: missing from sequential", path.display()));
        }
    }

    if errors.is_empty() {
        result.set("passed", true)?;
        result.set("files", current.len())?;
        result.set(
            "message",
            format!(
                "All {} files identical (deep structural comparison)",
                current.len()
            ),
        )?;
    } else {
        result.set("passed", false)?;
        result.set("files", current.len())?;
        result.set("message", errors.join("\n"))?;
    }

    Ok(result)
}

/// Run both diff approaches `iterations` times, doing deep structural comparison,
/// and write detailed results to `output_path` as JSON.
///
/// Call from Neovim (from the target repo's directory):
/// ```lua
/// require("difftastic-nvim.binary").get().benchmark_diff_methods("aded7aa..0342697", "git", 50, "/tmp/difft-bench.json")
/// ```
fn benchmark_diff_methods(
    _lua: &Lua,
    (range, vcs, iterations, output_path): (String, String, u32, String),
) -> LuaResult<()> {
    use std::io::Write;

    if vcs != "git" {
        return Err(LuaError::RuntimeError(
            "benchmark only supports git".into(),
        ));
    }

    let (old_ref, new_ref) = parse_git_range(&range);
    let range_arg = format!("{old_ref}..{new_ref}");
    let extra_args = vec![range_arg.as_str()];

    let mut current_times: Vec<u64> = Vec::new();
    let mut parallel_times: Vec<u64> = Vec::new();

    // First pass: deep comparison (run once, report all file-level details)
    let t1 = std::time::Instant::now();
    let current = run_git_diff(&extra_args).map_err(LuaError::RuntimeError)?;
    current_times.push(t1.elapsed().as_millis() as u64);

    let t2 = std::time::Instant::now();
    let parallel =
        run_git_diff_parallel(&extra_args, &old_ref, &new_ref).map_err(LuaError::RuntimeError)?;
    parallel_times.push(t2.elapsed().as_millis() as u64);

    // Build maps by normalized path
    let normalize_path = |p: &Path| -> PathBuf {
        let (_, new_path) = split_display_path(p);
        new_path
    };

    let current_map: HashMap<PathBuf, &difftastic::DifftFile> = current
        .iter()
        .map(|f| (normalize_path(&f.path), f))
        .collect();

    let parallel_map: HashMap<PathBuf, &difftastic::DifftFile> = parallel
        .iter()
        .map(|f| (normalize_path(&f.path), f))
        .collect();

    let mut all_paths: Vec<PathBuf> = current_map
        .keys()
        .chain(parallel_map.keys())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    all_paths.sort();

    // Deep comparison results for iteration 1
    let mut file_results: Vec<serde_json::Value> = Vec::new();
    let mut total_chunks = 0usize;
    let mut total_aligned_lines = 0usize;
    let mut total_changes = 0usize;
    let mut total_match = 0u32;
    let mut total_mismatch = 0u32;
    let mut only_current_count = 0u32;
    let mut only_parallel_count = 0u32;

    for path in &all_paths {
        match (current_map.get(path), parallel_map.get(path)) {
            (Some(c), Some(p)) => {
                let chunk_count = c.chunks.len();
                let aligned_count = c.aligned_lines.len();
                let change_count: usize = c
                    .chunks
                    .iter()
                    .flat_map(|ch| ch.iter())
                    .map(|dl| {
                        dl.lhs.as_ref().map_or(0, |s| s.changes.len())
                            + dl.rhs.as_ref().map_or(0, |s| s.changes.len())
                    })
                    .sum();

                total_chunks += chunk_count;
                total_aligned_lines += aligned_count;
                total_changes += change_count;

                let deep_result = deep_compare_files(c, p);
                let is_match = deep_result.is_none();

                if is_match {
                    total_match += 1;
                } else {
                    total_mismatch += 1;
                }

                file_results.push(serde_json::json!({
                    "path": path.to_string_lossy(),
                    "result": if is_match { "MATCH" } else { "MISMATCH" },
                    "first_difference": deep_result,
                    "status_current": format!("{:?}", c.status),
                    "status_parallel": format!("{:?}", p.status),
                    "language": &c.language,
                    "chunks": chunk_count,
                    "aligned_lines": aligned_count,
                    "changes": change_count,
                }));
            }
            (Some(c), None) => {
                only_current_count += 1;
                file_results.push(serde_json::json!({
                    "path": path.to_string_lossy(),
                    "result": "ONLY_IN_CURRENT",
                    "status": format!("{:?}", c.status),
                }));
            }
            (None, Some(p)) => {
                only_parallel_count += 1;
                file_results.push(serde_json::json!({
                    "path": path.to_string_lossy(),
                    "result": "ONLY_IN_PARALLEL",
                    "status": format!("{:?}", p.status),
                }));
            }
            (None, None) => unreachable!(),
        }
    }

    // Remaining iterations: just timing (deep comparison already done on iter 1)
    for _ in 1..iterations {
        let t = std::time::Instant::now();
        let _ = run_git_diff(&extra_args).map_err(LuaError::RuntimeError)?;
        current_times.push(t.elapsed().as_millis() as u64);

        let t = std::time::Instant::now();
        let _ = run_git_diff_parallel(&extra_args, &old_ref, &new_ref)
            .map_err(LuaError::RuntimeError)?;
        parallel_times.push(t.elapsed().as_millis() as u64);
    }

    // Compute timing statistics
    let stats = |times: &mut Vec<u64>| -> serde_json::Value {
        times.sort();
        let len = times.len() as f64;
        let sum: u64 = times.iter().sum();
        let mean = sum as f64 / len;
        let median = if times.len() % 2 == 0 {
            (times[times.len() / 2 - 1] + times[times.len() / 2]) as f64 / 2.0
        } else {
            times[times.len() / 2] as f64
        };
        let variance: f64 = times.iter().map(|&t| (t as f64 - mean).powi(2)).sum::<f64>() / len;
        let stddev = variance.sqrt();
        let p5 = times[(len * 0.05) as usize];
        let p95 = times[(len * 0.95) as usize];

        serde_json::json!({
            "min": times[0],
            "max": times[times.len() - 1],
            "mean": format!("{:.1}", mean),
            "median": format!("{:.1}", median),
            "stddev": format!("{:.1}", stddev),
            "p5": p5,
            "p95": p95,
            "all_ms": times,
        })
    };

    let current_stats = stats(&mut current_times);
    let parallel_stats = stats(&mut parallel_times);

    let output = serde_json::json!({
        "range": range,
        "iterations": iterations,
        "deep_comparison": {
            "total_files": all_paths.len(),
            "files_matched": total_match,
            "files_mismatched": total_mismatch,
            "only_in_current": only_current_count,
            "only_in_parallel": only_parallel_count,
            "total_chunks_compared": total_chunks,
            "total_aligned_lines_compared": total_aligned_lines,
            "total_changes_compared": total_changes,
            "all_identical": total_mismatch == 0 && only_current_count == 0 && only_parallel_count == 0,
        },
        "timing": {
            "current": current_stats,
            "parallel": parallel_stats,
        },
        "per_file": file_results,
    });

    let mut f = std::fs::File::create(&output_path)
        .map_err(|e| LuaError::RuntimeError(format!("Failed to create {output_path}: {e}")))?;
    f.write_all(serde_json::to_string_pretty(&output).unwrap().as_bytes())
        .map_err(|e| LuaError::RuntimeError(format!("Failed to write: {e}")))?;

    Ok(())
}

/// Gets the merge-base of two git refs.
fn git_merge_base(a: &str, b: &str) -> Option<String> {
    Command::new("git")
        .args(["merge-base", a, b])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Expands diff display paths for renames/moves into concrete old/new paths.
///
/// Handles common formats:
/// - `old/path => new/path`
/// - `old/path -> new/path`
/// - `src/{old => new}.rs`
fn split_display_path(path: &Path) -> (PathBuf, PathBuf) {
    let raw = path.to_string_lossy();

    if let (Some(open), Some(close)) = (raw.find('{'), raw.rfind('}'))
        && close > open
    {
        let prefix = &raw[..open];
        let suffix = &raw[(close + 1)..];
        let inner = &raw[(open + 1)..close];

        for arrow in [" => ", " -> "] {
            if let Some((lhs, rhs)) = inner.split_once(arrow)
                && !lhs.trim().is_empty()
                && !rhs.trim().is_empty()
            {
                let old_path = format!("{prefix}{}{suffix}", lhs.trim());
                let new_path = format!("{prefix}{}{suffix}", rhs.trim());
                return (PathBuf::from(old_path), PathBuf::from(new_path));
            }
        }
    }

    for arrow in [" => ", " -> "] {
        if let Some((lhs, rhs)) = raw.split_once(arrow)
            && !lhs.trim().is_empty()
            && !rhs.trim().is_empty()
        {
            return (PathBuf::from(lhs.trim()), PathBuf::from(rhs.trim()));
        }
    }

    (path.to_path_buf(), path.to_path_buf())
}

fn prepare_file_for_display(
    file: &mut difftastic::DifftFile,
    stats: &FileStats,
) -> (Option<(u32, u32)>, PathBuf, PathBuf, Option<PathBuf>) {
    let file_stats = stats.get(&file.path).copied();
    let (old_path, new_path) = split_display_path(&file.path);

    let moved_from = if old_path != new_path {
        file.path = new_path.clone();
        file.status = difftastic::Status::Created;
        Some(old_path.clone())
    } else {
        None
    };

    (file_stats, old_path, new_path, moved_from)
}

fn process_prepared_file(
    file: difftastic::DifftFile,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    file_stats: Option<(u32, u32)>,
    moved_from: Option<PathBuf>,
) -> processor::DisplayFile {
    let mut display = processor::process_file(file, old_lines, new_lines, file_stats);
    display.moved_from = moved_from;
    display
}

fn parse_jj_summary_rename(line: &str) -> Option<(PathBuf, PathBuf)> {
    let renamed = line.trim().strip_prefix("R ")?;
    let (old_path, new_path) = split_display_path(Path::new(renamed));
    (old_path != new_path).then_some((old_path, new_path))
}

fn parse_jj_summary_renames(output: &str) -> HashMap<PathBuf, PathBuf> {
    output
        .lines()
        .filter_map(parse_jj_summary_rename)
        .map(|(old_path, new_path)| (new_path, old_path))
        .collect()
}

fn parse_git_name_status_rename(line: &str) -> Option<(PathBuf, PathBuf)> {
    let mut parts = line.trim().split('\t');
    let status = parts.next()?;
    if !status.starts_with('R') {
        return None;
    }

    let old_path = parts.next()?.trim();
    let new_path = parts.next()?.trim();
    if old_path.is_empty() || new_path.is_empty() {
        return None;
    }

    Some((PathBuf::from(old_path), PathBuf::from(new_path)))
}

fn parse_git_name_status_renames(output: &str) -> HashMap<PathBuf, PathBuf> {
    output
        .lines()
        .filter_map(parse_git_name_status_rename)
        .map(|(old_path, new_path)| (new_path, old_path))
        .collect()
}

fn git_rename_map(extra_args: &[&str]) -> HashMap<PathBuf, PathBuf> {
    let mut cmd = Command::new("git");
    cmd.args(["diff", "--name-status", "-M"]);
    cmd.args(extra_args);

    let output = cmd.output().ok();
    let Some(output) = output.filter(|o| o.status.success()) else {
        return HashMap::new();
    };

    parse_git_name_status_renames(&String::from_utf8_lossy(&output.stdout))
}

fn jj_rename_map(mode: &DiffMode) -> HashMap<PathBuf, PathBuf> {
    let mut cmd = Command::new("jj");
    cmd.arg("diff");

    match mode {
        DiffMode::Range(revset) => {
            cmd.arg("-r").arg(revset);
        }
        DiffMode::Unstaged | DiffMode::WorkingTree(_) => {}
        DiffMode::Staged | DiffMode::StagedVsCommit(_) => {
            cmd.args(["-r", "@"]);
        }
    }

    let output = cmd.arg("--summary").output().ok();
    let Some(output) = output.filter(|o| o.status.success()) else {
        return HashMap::new();
    };

    parse_jj_summary_renames(&String::from_utf8_lossy(&output.stdout))
}

/// Parses a git commit range into `(old_commit, new_commit)` references.
///
/// Handles single commits, `A..B` ranges, and `A...B` (merge-base) ranges.
#[inline]
fn parse_git_range(range: &str) -> (String, String) {
    if let Some((a, b)) = range.split_once("...") {
        let base = git_merge_base(a, b).unwrap_or_else(|| format!("{a}^"));
        (base, b.to_string())
    } else if let Some((old, new)) = range.split_once("..") {
        (old.to_string(), new.to_string())
    } else {
        (format!("{range}^"), range.to_string())
    }
}

/// The type of diff to perform.
enum DiffMode {
    /// A commit range (e.g., "HEAD^..HEAD" for git, "@" for jj).
    Range(String),
    /// Working tree vs index (git) or working copy vs @ (jj).
    Unstaged,
    /// Index vs HEAD (git) or @- vs @ (jj).
    Staged,
    /// Working tree vs a specific commit.
    WorkingTree(String),
    /// Index (staged) vs a specific commit.
    StagedVsCommit(String),
}

/// Strategy for fetching old/new file content based on diff mode and VCS.
/// Constructed once before parallel processing, then shared across threads.
enum ContentFetcher {
    GitRange(String, String),
    JjRange(String, String),
    GitUnstaged,
    JjUnstaged,
    GitStaged,
    JjStaged,
    GitWorkingTree(String),
    GitStagedVsCommit(String),
}

impl ContentFetcher {
    /// Create the appropriate fetcher for a given mode and VCS.
    fn new(mode: &DiffMode, vcs: &str) -> Self {
        match (mode, vcs) {
            (DiffMode::Range(range), "git") => {
                let (old_ref, new_ref) = parse_git_range(range);
                Self::GitRange(old_ref, new_ref)
            }
            (DiffMode::Range(range), _) => {
                let (old_ref, new_ref) = parse_jj_range(range)
                    .unwrap_or_else(|| (format!("roots({range})-"), format!("heads({range})")));
                Self::JjRange(old_ref, new_ref)
            }
            (DiffMode::Unstaged, "git") => Self::GitUnstaged,
            (DiffMode::Unstaged, _) => Self::JjUnstaged,
            (DiffMode::Staged, "git") => Self::GitStaged,
            (DiffMode::Staged, _) => Self::JjStaged,
            (DiffMode::WorkingTree(commit), "git") => Self::GitWorkingTree(commit.clone()),
            (DiffMode::WorkingTree(_), _) => Self::JjUnstaged,
            (DiffMode::StagedVsCommit(commit), "git") => Self::GitStagedVsCommit(commit.clone()),
            (DiffMode::StagedVsCommit(_), _) => Self::JjStaged,
        }
    }

    /// Fetch old and new file content for a given file path pair.
    fn fetch(&self, old_path: &Path, new_path: &Path) -> (Vec<String>, Vec<String>) {
        match self {
            Self::GitRange(old_ref, new_ref) => (
                into_lines(git_file_content(old_ref, old_path)),
                into_lines(git_file_content(new_ref, new_path)),
            ),
            Self::JjRange(old_ref, new_ref) => (
                into_lines(jj_file_content(old_ref, old_path)),
                into_lines(jj_file_content(new_ref, new_path)),
            ),
            Self::GitUnstaged => (
                into_lines(git_index_content(old_path)),
                into_lines(working_tree_content_for_vcs(new_path, "git")),
            ),
            Self::JjUnstaged => (
                into_lines(jj_file_content("@", old_path)),
                into_lines(working_tree_content_for_vcs(new_path, "jj")),
            ),
            Self::GitStaged => (
                into_lines(git_file_content("HEAD", old_path)),
                into_lines(git_index_content(new_path)),
            ),
            Self::JjStaged => (
                into_lines(jj_file_content("@-", old_path)),
                into_lines(jj_file_content("@", new_path)),
            ),
            Self::GitWorkingTree(commit) => (
                into_lines(git_file_content(commit, old_path)),
                into_lines(working_tree_content_for_vcs(new_path, "git")),
            ),
            Self::GitStagedVsCommit(commit) => (
                into_lines(git_file_content(commit, old_path)),
                into_lines(git_index_content(new_path)),
            ),
        }
    }
}

/// Fetches file content from the working tree, using the appropriate VCS root.
fn working_tree_content_for_vcs(path: &Path, vcs: &str) -> Option<String> {
    let root = if vcs == "git" { git_root() } else { jj_root() }?;
    std::fs::read_to_string(root.join(path)).ok()
}

/// Unified implementation for running difftastic with any diff mode.
/// Handles git and jj VCS, fetches file contents, and processes files in parallel.
///
/// For git modes, runs diff/stats/rename-detection subprocesses concurrently
/// using `std::thread::scope` (scoped threads can borrow local data without `'static`).
fn run_diff_impl(lua: &Lua, mode: DiffMode, vcs: &str) -> LuaResult<LuaTable> {
    let (files, stats, renames) = if vcs == "git" {
        // Compute git args from mode before entering the scope so borrows are clear.
        // For Range mode, parse once into an owned string so it outlives the scope.
        let (old_ref, new_ref): (Option<String>, Option<String>) = match &mode {
            DiffMode::Range(range) => {
                let (o, n) = parse_git_range(range);
                (Some(o), Some(n))
            }
            DiffMode::Unstaged => (None, None),
            DiffMode::Staged => (Some("HEAD".to_string()), None),
            DiffMode::WorkingTree(commit) => (Some(commit.clone()), None),
            DiffMode::StagedVsCommit(commit) => (Some(commit.clone()), None),
        };

        // Build owned extra_args so they don't borrow old_ref/new_ref,
        // allowing old_ref/new_ref to be moved into the files closure.
        let extra_args: Vec<String> = match &mode {
            DiffMode::Range(_) => {
                let o = old_ref.as_deref().unwrap();
                let n = new_ref.as_deref().unwrap();
                vec![format!("{o}..{n}")]
            }
            DiffMode::Unstaged => vec![],
            DiffMode::Staged => vec!["--cached".to_string()],
            DiffMode::WorkingTree(_) => vec![old_ref.as_deref().unwrap().to_string()],
            DiffMode::StagedVsCommit(_) => vec![
                "--cached".to_string(),
                old_ref.as_deref().unwrap().to_string(),
            ],
        };

        // Run diff + stats + renames concurrently.
        // Range mode uses parallel diff (calls difft directly per file via rayon).
        // Non-range modes use sequential diff (git ext-diff) — they're fast (few files).
        let stats_args = extra_args.clone();
        let renames_args = extra_args.clone();
        let (files_result, stats, renames) = std::thread::scope(|s| {
            let files_handle = s.spawn(move || {
                let refs: Vec<&str> = extra_args.iter().map(|s| s.as_str()).collect();
                if let (Some(o), Some(n)) = (old_ref, new_ref) {
                    run_git_diff_parallel(&refs, &o, &n)
                } else {
                    run_git_diff(&refs)
                }
            });
            let stats_handle = s.spawn(|| {
                let refs: Vec<&str> = stats_args.iter().map(|s| s.as_str()).collect();
                git_diff_stats(&refs)
            });
            let renames_handle = s.spawn(|| {
                let refs: Vec<&str> = renames_args.iter().map(|s| s.as_str()).collect();
                git_rename_map(&refs)
            });
            (
                files_handle.join().unwrap(),
                stats_handle.join().unwrap(),
                renames_handle.join().unwrap(),
            )
        });
        let files = files_result.map_err(LuaError::RuntimeError)?;
        (files, stats, renames)
    } else {
        // jj modes — also parallelize where possible
        match &mode {
            DiffMode::Range(range) => {
                let (files_result, stats, renames) = std::thread::scope(|s| {
                    let files_handle = s.spawn(|| run_jj_diff(range));
                    let stats_handle = s.spawn(|| jj_diff_stats(range));
                    let renames_handle = s.spawn(|| jj_rename_map(&mode));
                    (
                        files_handle.join().unwrap(),
                        stats_handle.join().unwrap(),
                        renames_handle.join().unwrap(),
                    )
                });
                let files = files_result.map_err(LuaError::RuntimeError)?;
                (files, stats, renames)
            }
            DiffMode::Unstaged | DiffMode::WorkingTree(_) => {
                let files = run_jj_diff_uncommitted().map_err(LuaError::RuntimeError)?;
                let stats = jj_diff_stats_uncommitted();
                (files, stats, HashMap::new())
            }
            DiffMode::Staged | DiffMode::StagedVsCommit(_) => {
                let (files_result, stats, renames) = std::thread::scope(|s| {
                    let files_handle = s.spawn(|| run_jj_diff("@"));
                    let stats_handle = s.spawn(|| jj_diff_stats("@"));
                    let renames_handle = s.spawn(|| jj_rename_map(&mode));
                    (
                        files_handle.join().unwrap(),
                        stats_handle.join().unwrap(),
                        renames_handle.join().unwrap(),
                    )
                });
                let files = files_result.map_err(LuaError::RuntimeError)?;
                (files, stats, renames)
            }
        }
    };

    // Cross-check: every file in git diff --numstat must be in our file list.
    // numstat is a separate git command — if it sees files we don't, we missed something.
    // (numstat may have fewer files than us since it skips binary files — that's fine.)
    if vcs == "git" {
        let file_paths: HashSet<&Path> = files.iter().map(|f| f.path.as_path()).collect();
        for stat_path in stats.keys() {
            let (_, normalized) = split_display_path(stat_path);
            if !file_paths.contains(normalized.as_path()) {
                return Err(LuaError::RuntimeError(format!(
                    "Integrity: git numstat reports {} but it's missing from diff output",
                    stat_path.display()
                )));
            }
        }
    }

    // Process files in parallel (rayon) — depends on files + stats
    let fetcher = ContentFetcher::new(&mode, vcs);

    let mut display_files: Vec<_> = files
        .into_par_iter()
        .map(|mut file| {
            let (file_stats, old_path, new_path, moved_from) =
                prepare_file_for_display(&mut file, &stats);
            let (old_lines, new_lines) = fetcher.fetch(&old_path, &new_path);
            process_prepared_file(file, old_lines, new_lines, file_stats, moved_from)
        })
        .collect();

    // Apply renames — depends on display_files + renames
    if !renames.is_empty() {
        let old_paths: HashSet<PathBuf> = renames.values().cloned().collect();

        display_files = display_files
            .into_iter()
            .filter_map(|mut file| {
                if let Some(old_path) = renames.get(&file.path) {
                    file.moved_from = Some(old_path.clone());
                    file.status = difftastic::Status::Created;
                }

                if file.status == difftastic::Status::Deleted && old_paths.contains(&file.path) {
                    return None;
                }

                Some(file)
            })
            .collect();
    }

    // === Display-level integrity checks ===
    // Catches bugs in our processing pipeline — these should never fire in normal operation.
    for file in &display_files {
        // 1. No duplicate paths
        // (checked at git level above, but verify post-rename too)

        // 2. aligned_lines.len() must match rows.len()
        if file.aligned_lines.len() != file.rows.len() {
            return Err(LuaError::RuntimeError(format!(
                "Integrity: {}: aligned_lines.len() ({}) != rows.len() ({})",
                file.path.display(),
                file.aligned_lines.len(),
                file.rows.len()
            )));
        }

        // 3. Filler consistency: filler sides must have empty content
        for (ri, row) in file.rows.iter().enumerate() {
            if row.left.is_filler && !row.left.content.is_empty() {
                return Err(LuaError::RuntimeError(format!(
                    "Integrity: {}: row {} left is filler but has content",
                    file.path.display(), ri
                )));
            }
            if row.right.is_filler && !row.right.content.is_empty() {
                return Err(LuaError::RuntimeError(format!(
                    "Integrity: {}: row {} right is filler but has content",
                    file.path.display(), ri
                )));
            }
        }

        // 4. Hunk starts must be within row bounds and sorted
        for (i, &start) in file.hunk_starts.iter().enumerate() {
            if start as usize >= file.rows.len() {
                return Err(LuaError::RuntimeError(format!(
                    "Integrity: {}: hunk_start[{}] = {} >= rows.len() ({})",
                    file.path.display(), i, start, file.rows.len()
                )));
            }
            if i > 0 && start <= file.hunk_starts[i - 1] {
                return Err(LuaError::RuntimeError(format!(
                    "Integrity: {}: hunk_starts not sorted: [{}]={} <= [{}]={}",
                    file.path.display(), i, start, i - 1, file.hunk_starts[i - 1]
                )));
            }
        }

        // 5. Created file (without moved_from): left side should all be filler
        if file.status == difftastic::Status::Created && file.moved_from.is_none() {
            for (ri, row) in file.rows.iter().enumerate() {
                if !row.left.is_filler {
                    return Err(LuaError::RuntimeError(format!(
                        "Integrity: {}: created (non-rename) file has non-filler left at row {}",
                        file.path.display(), ri
                    )));
                }
            }
        }

        // 6. Deleted file: right side should all be filler
        if file.status == difftastic::Status::Deleted {
            for (ri, row) in file.rows.iter().enumerate() {
                if !row.right.is_filler {
                    return Err(LuaError::RuntimeError(format!(
                        "Integrity: {}: deleted file has non-filler right at row {}",
                        file.path.display(), ri
                    )));
                }
            }
        }
    }

    // 7. No duplicate paths in final output
    {
        let mut seen_paths = HashSet::with_capacity(display_files.len());
        for file in &display_files {
            if !seen_paths.insert(&file.path) {
                return Err(LuaError::RuntimeError(format!(
                    "Integrity: duplicate path {} in output",
                    file.path.display()
                )));
            }
        }
    }

    let files_table = lua.create_table()?;
    for (i, file) in display_files.into_iter().enumerate() {
        files_table.set(i + 1, file.into_lua(lua)?)?;
    }

    let result = lua.create_table()?;
    result.set("files", files_table)?;
    Ok(result)
}

/// Runs difftastic for a commit range.
fn run_diff(lua: &Lua, (range, vcs): (String, String)) -> LuaResult<LuaTable> {
    run_diff_impl(lua, DiffMode::Range(range), &vcs)
}

/// Runs difftastic for working tree vs index.
fn run_diff_unstaged(lua: &Lua, vcs: String) -> LuaResult<LuaTable> {
    run_diff_impl(lua, DiffMode::Unstaged, &vcs)
}

/// Runs difftastic for index vs HEAD.
fn run_diff_staged(lua: &Lua, vcs: String) -> LuaResult<LuaTable> {
    run_diff_impl(lua, DiffMode::Staged, &vcs)
}

/// Runs difftastic comparing working tree against a specific commit.
fn run_diff_working_tree(lua: &Lua, (commit, vcs): (String, String)) -> LuaResult<LuaTable> {
    run_diff_impl(lua, DiffMode::WorkingTree(commit), &vcs)
}

/// Runs difftastic comparing index (staged) against a specific commit.
fn run_diff_staged_vs_commit(lua: &Lua, (commit, vcs): (String, String)) -> LuaResult<LuaTable> {
    run_diff_impl(lua, DiffMode::StagedVsCommit(commit), &vcs)
}

/// Gets untracked files from git (excluding ignored files).
/// Uses `--full-name` for root-relative paths and `:/` pathspec to search
/// the entire repo regardless of cwd.
fn git_untracked_files() -> Vec<PathBuf> {
    let output = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "--full-name", ":/"])
        .output()
        .ok();

    let Some(output) = output.filter(|o| o.status.success()) else {
        return Vec::new();
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Maps file extensions to difftastic language names for treesitter highlighting.
fn language_from_ext(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "Rust",
        Some("lua") => "Lua",
        Some("toml") => "TOML",
        Some("json") => "JSON",
        Some("js" | "mjs" | "cjs") => "JavaScript",
        Some("ts" | "mts" | "cts") => "TypeScript",
        Some("py") => "Python",
        Some("go") => "Go",
        Some("c" | "h") => "C",
        Some("cpp" | "cc" | "cxx" | "hpp") => "C++",
        Some("java") => "Java",
        Some("rb") => "Ruby",
        Some("sh" | "bash" | "zsh") => "Shell",
        Some("md") => "Markdown",
        Some("yml" | "yaml") => "YAML",
        Some("html" | "htm") => "HTML",
        Some("css") => "CSS",
        Some("clj" | "cljs") => "Clojure",
        _ => "Text",
    }
    .to_string()
}

/// Returns DisplayFile tables for all untracked files.
/// Separate from run_diff_* so Lua controls when/how to include them.
fn get_untracked_files(lua: &Lua, vcs: String) -> LuaResult<LuaTable> {
    let untracked = if vcs == "git" {
        git_untracked_files()
    } else {
        // jj tracks everything — no concept of "untracked"
        return lua.create_sequence_from(Vec::<LuaValue>::new());
    };

    let root = git_root();
    let display_files: Vec<_> = untracked
        .into_par_iter()
        .filter_map(|path| {
            let abs_path = root.as_ref()?.join(&path);
            let content = std::fs::read_to_string(&abs_path).ok()?;
            let new_lines: Vec<String> = content.lines().map(String::from).collect();
            let num_lines = new_lines.len() as u32;
            let language = language_from_ext(&path);
            Some(processor::process_file(
                difftastic::DifftFile {
                    path,
                    language,
                    status: difftastic::Status::Created,
                    aligned_lines: vec![],
                    chunks: vec![],
                },
                vec![],
                new_lines,
                Some((num_lines, 0)),
            ))
        })
        .collect();

    let files_table = lua.create_table()?;
    for (i, file) in display_files.into_iter().enumerate() {
        files_table.set(i + 1, file.into_lua(lua)?)?;
    }
    Ok(files_table)
}

/// Creates the Lua module exports. Called by mlua when loaded via `require("difftastic_nvim")`.
#[mlua::lua_module]
fn difftastic_nvim(lua: &Lua) -> LuaResult<LuaTable> {
    let exports = lua.create_table()?;
    exports.set(
        "run_diff",
        lua.create_function(|lua, args: (String, String)| run_diff(lua, args))?,
    )?;
    exports.set(
        "run_diff_unstaged",
        lua.create_function(|lua, vcs: String| run_diff_unstaged(lua, vcs))?,
    )?;
    exports.set(
        "run_diff_staged",
        lua.create_function(|lua, vcs: String| run_diff_staged(lua, vcs))?,
    )?;
    exports.set(
        "run_diff_working_tree",
        lua.create_function(|lua, args: (String, String)| run_diff_working_tree(lua, args))?,
    )?;
    exports.set(
        "run_diff_staged_vs_commit",
        lua.create_function(|lua, args: (String, String)| run_diff_staged_vs_commit(lua, args))?,
    )?;
    exports.set(
        "get_untracked_files",
        lua.create_function(|lua, vcs: String| get_untracked_files(lua, vcs))?,
    )?;
    exports.set(
        "benchmark_diff_methods",
        lua.create_function(|lua, args: (String, String, u32, String)| {
            benchmark_diff_methods(lua, args)
        })?,
    )?;
    exports.set(
        "verify_diff",
        lua.create_function(|lua, args: (String, String)| verify_diff(lua, args))?,
    )?;
    Ok(exports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_into_lines_with_content() {
        let lines = into_lines(Some("line1\nline2\nline3".to_string()));
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn test_into_lines_empty() {
        let lines = into_lines(None);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_into_lines_single_line() {
        let lines = into_lines(Some("single".to_string()));
        assert_eq!(lines, vec!["single"]);
    }

    #[test]
    fn test_parse_git_range_single_commit() {
        let (old, new) = parse_git_range("abc123");
        assert_eq!(old, "abc123^");
        assert_eq!(new, "abc123");
    }

    #[test]
    fn test_parse_git_range_double_dot() {
        let (old, new) = parse_git_range("main..feature");
        assert_eq!(old, "main");
        assert_eq!(new, "feature");
    }

    #[test]
    fn test_parse_git_range_empty_left() {
        let (old, new) = parse_git_range("..HEAD");
        assert_eq!(old, "");
        assert_eq!(new, "HEAD");
    }

    #[test]
    fn test_parse_jj_range_double_dot() {
        let (old, new) = parse_jj_range("main@origin..@").unwrap();
        assert_eq!(old, "main@origin");
        assert_eq!(new, "@");
    }

    #[test]
    fn test_parse_jj_range_non_range() {
        assert!(parse_jj_range("@").is_none());
    }

    #[test]
    fn test_split_display_path_plain() {
        let (old, new) = split_display_path(Path::new("src/lib.rs"));
        assert_eq!(old, PathBuf::from("src/lib.rs"));
        assert_eq!(new, PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn test_split_display_path_arrow() {
        let (old, new) = split_display_path(Path::new("src/old.rs => src/new.rs"));
        assert_eq!(old, PathBuf::from("src/old.rs"));
        assert_eq!(new, PathBuf::from("src/new.rs"));
    }

    #[test]
    fn test_split_display_path_brace() {
        let (old, new) = split_display_path(Path::new("src/{old => new}.rs"));
        assert_eq!(old, PathBuf::from("src/old.rs"));
        assert_eq!(new, PathBuf::from("src/new.rs"));
    }

    #[test]
    fn test_parse_jj_summary_rename_simple() {
        let parsed = parse_jj_summary_rename("R src/old.rs => src/new.rs").unwrap();
        assert_eq!(parsed.0, PathBuf::from("src/old.rs"));
        assert_eq!(parsed.1, PathBuf::from("src/new.rs"));
    }

    #[test]
    fn test_parse_jj_summary_rename_brace() {
        let parsed = parse_jj_summary_rename("R src/{old => new}.rs").unwrap();
        assert_eq!(parsed.0, PathBuf::from("src/old.rs"));
        assert_eq!(parsed.1, PathBuf::from("src/new.rs"));
    }

    #[test]
    fn test_parse_jj_summary_renames_map() {
        let renames = parse_jj_summary_renames("R a.txt => b.txt\nA c.txt\n");
        assert_eq!(
            renames.get(Path::new("b.txt")),
            Some(&PathBuf::from("a.txt"))
        );
        assert!(!renames.contains_key(Path::new("c.txt")));
    }

    #[test]
    fn test_parse_git_name_status_rename() {
        let parsed = parse_git_name_status_rename("R100\tsrc/old.rs\tsrc/new.rs").unwrap();
        assert_eq!(parsed.0, PathBuf::from("src/old.rs"));
        assert_eq!(parsed.1, PathBuf::from("src/new.rs"));
    }

    #[test]
    fn test_parse_git_name_status_renames_map() {
        let renames = parse_git_name_status_renames("R090\ta.txt\tb.txt\nM c.txt\n");
        assert_eq!(
            renames.get(Path::new("b.txt")),
            Some(&PathBuf::from("a.txt"))
        );
        assert!(!renames.contains_key(Path::new("c.txt")));
    }
}
