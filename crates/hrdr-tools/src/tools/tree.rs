use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

// ---- tree ----

pub struct TreeTool;

#[derive(Deserialize)]
struct TreeArgs {
    #[serde(default)]
    path: Option<String>,
    /// Max directory depth to descend (default 3, max 10).
    #[serde(default = "default_max_depth")]
    max_depth: usize,
    /// Max entries to return per directory before summarizing the rest (default 200).
    #[serde(default = "default_max_entries")]
    max_entries: usize,
    /// Also show hidden files/dirs (dotfiles). Skipped by default.
    #[serde(default)]
    hidden: bool,
}

fn default_max_depth() -> usize {
    3
}
fn default_max_entries() -> usize {
    200
}

/// A path entry collected from the walker.
struct Collected {
    /// Relative path components from root.
    components: Vec<String>,
    /// Whether this is a directory.
    is_dir: bool,
    /// Whether this is a symlink.
    is_symlink: bool,
}

#[async_trait]
impl Tool for TreeTool {
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "tree"
    }
    fn description(&self) -> &'static str {
        "Show a directory tree. By default, hidden files/dirs (dotfiles, e.g. `.github/`) \
         are hidden and .gitignore'd paths (e.g. `target/`, `node_modules/`) are honored \
         (excluded); set `hidden` to include dotfiles. Directories get a trailing `/`, \
         symlinks a trailing `@`. Use `max_depth` to limit recursion (default 3, max 10) \
         and `max_entries` to cap per-directory entries (default 200)."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to tree (default: cwd)."
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Max directory depth (default 3, max 10)."
                },
                "max_entries": {
                    "type": "integer",
                    "description": "Max entries shown per directory before summarizing the rest (default 200)."
                },
                "hidden": {
                    "type": "boolean",
                    "description": "Also show hidden files/dirs (dotfiles). Skipped by default (default false)."
                }
            }
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: TreeArgs = crate::tool_args("tree", args)?;
        let depth = a.max_depth.min(10);
        let max_entries = a.max_entries;

        let root = ctx.resolve(a.path.as_deref().unwrap_or("."));

        // Collect entries from the ignore walker.
        let entries = collect_entries(&root, depth, max_entries, a.hidden)?;

        // Render to a string.
        let root_label = if a.path.as_deref().is_none_or(|p| p == ".") {
            ".".to_string()
        } else {
            root.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.to_string_lossy().to_string())
        };
        let out = render_tree(&root_label, &entries);

        Ok(truncate(&out, ctx.max_output))
    }
}

/// Walk `root` with `ignore::WalkBuilder` (honours `.gitignore`/`.ignore`, and
/// hides dotfiles unless `hidden` is set), returning a sorted list of entries
/// capped per-directory at `max_entries`.
fn collect_entries(
    root: &Path,
    max_depth: usize,
    max_entries: usize,
    hidden: bool,
) -> Result<Vec<Collected>> {
    if max_depth == 0 {
        return Ok(Vec::new());
    }

    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(max_depth))
        .hidden(!hidden)
        .build();

    // Group entries by their parent directory path for capping.
    // Map: parent path -> list of (name, is_dir, is_symlink)
    let mut by_parent: BTreeMap<PathBuf, Vec<(String, bool, bool)>> = BTreeMap::new();

    for entry in walker.flatten() {
        let path = entry.path();
        if path == root {
            continue;
        }

        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let is_symlink = entry.file_type().is_some_and(|t| t.is_symlink());

        let parent = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());

        by_parent
            .entry(parent)
            .or_default()
            .push((name, is_dir, is_symlink));
    }

    // Sort and cap each parent's children.
    for children in by_parent.values_mut() {
        children.sort_by(|(a_name, a_dir, _), (b_name, b_dir, _)| {
            // Dirs first, then alphabetical.
            b_dir.cmp(a_dir).then_with(|| a_name.cmp(b_name))
        });
        if children.len() > max_entries {
            let more = children.len() - max_entries;
            children.truncate(max_entries);
            children.push((format!("… [{more} more entries]"), false, false));
        }
    }

    // Now build the Collected entries by traversing the tree.
    // We need to emit entries in the right order (dirs-first within each level)
    // and only include directories that have at least one descendant in the set.
    let kept_paths: BTreeSet<PathBuf> = by_parent
        .keys()
        .flat_map(|p| {
            let mut paths = Vec::new();
            let mut cur = p.as_path();
            while cur != root {
                paths.push(cur.to_path_buf());
                cur = cur.parent().unwrap_or(root);
            }
            paths
        })
        .collect();

    // Build the result: recursively walk from root, emitting dirs/files
    // in sorted order, only descending into dirs that have kept entries.
    let mut result: Vec<Collected> = Vec::new();
    build_sorted(root, &[], &by_parent, &kept_paths, max_depth, &mut result);

    Ok(result)
}

/// Recursively build a sorted list of Collected entries.
fn build_sorted(
    dir: &Path,
    prefix: &[String],
    by_parent: &BTreeMap<PathBuf, Vec<(String, bool, bool)>>,
    kept_dirs: &BTreeSet<PathBuf>,
    remaining_depth: usize,
    out: &mut Vec<Collected>,
) {
    if remaining_depth == 0 {
        return;
    }

    let Some(children) = by_parent.get(dir) else {
        return;
    };

    for (name, is_dir, is_symlink) in children {
        if name.starts_with("… [") {
            // The "more entries" sentinel — emit as a file marker.
            let mut comps = prefix.to_vec();
            comps.push(name.clone());
            out.push(Collected {
                components: comps,
                is_dir: false,
                is_symlink: false,
            });
            continue;
        }

        let mut comps = prefix.to_vec();
        comps.push(name.clone());

        let child_path = dir.join(name);

        if *is_dir && kept_dirs.contains(&child_path) {
            // Directory with children: emit and recurse.
            out.push(Collected {
                components: comps,
                is_dir: true,
                is_symlink: *is_symlink,
            });
            let mut child_prefix = prefix.to_vec();
            child_prefix.push(name.clone());
            build_sorted(
                &child_path,
                &child_prefix,
                by_parent,
                kept_dirs,
                remaining_depth - 1,
                out,
            );
        } else if *is_dir {
            // Empty directory (no kept children beyond this): emit as leaf dir.
            out.push(Collected {
                components: comps,
                is_dir: true,
                is_symlink: *is_symlink,
            });
        } else {
            out.push(Collected {
                components: comps,
                is_dir: false,
                is_symlink: *is_symlink,
            });
        }
    }
}

/// Render the collected entries as a tree with box-drawing characters.
///
/// `entries` is in DFS pre-order (see `build_sorted`): a node's whole subtree is
/// contiguous and immediately follows it. Whether entry `i` is the *last* child
/// of its parent is NOT decided by comparing depth to the very next entry
/// (`i`'s own children, if any, are deeper and would be misread as "more
/// siblings follow"). It's decided by the first LATER entry whose depth is
/// `<=` `i`'s own depth: if that entry doesn't exist, or is strictly shallower
/// (we popped out of `i`'s parent entirely), `i` was the last child; if it's
/// exactly as deep, that's a sibling, so `i` was not last.
///
/// We compute that "first later entry at depth `<=` mine" for every `i` in one
/// right-to-left pass with a monotonic stack (classic "next smaller-or-equal
/// element" — O(n) total, each index pushed/popped once), then render in a
/// second left-to-right pass carrying a small stack of each open ancestor's
/// own last-child flag (that's what decides whether its column draws a
/// continuation bar `│` or a blank gap).
fn render_tree(root_label: &str, entries: &[Collected]) -> String {
    let mut buf = format!("{}/\n", root_label);
    if entries.is_empty() {
        return buf;
    }

    let n = entries.len();
    let depths: Vec<usize> = entries.iter().map(|e| e.components.len()).collect();

    // is_last[i] = true iff the next entry at depth <= depths[i] (if any) is
    // strictly shallower — i.e. entry i closes out its parent's child list.
    let mut is_last = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for i in (0..n).rev() {
        while let Some(&top) = stack.last() {
            if depths[top] > depths[i] {
                stack.pop();
            } else {
                break;
            }
        }
        is_last[i] = match stack.last() {
            None => true,
            Some(&j) => depths[j] < depths[i],
        };
        stack.push(i);
    }

    // ancestor_is_last[d] (0-indexed) is the last-child flag of the ancestor
    // that owns prefix column `d`, i.e. the ancestor at depth `d + 1`.
    let mut ancestor_is_last: Vec<bool> = Vec::new();
    for i in 0..n {
        let depth = depths[i];
        // Pop back to i's parent's ancestor chain (length depth - 1).
        ancestor_is_last.truncate(depth - 1);

        for &al in &ancestor_is_last {
            buf.push_str(if al { "    " } else { "│   " });
        }

        buf.push_str(if is_last[i] {
            "└── "
        } else {
            "├── "
        });

        let name = &entries[i].components[depth - 1];
        let suffix = if entries[i].is_symlink {
            "@"
        } else if entries[i].is_dir {
            "/"
        } else {
            ""
        };
        buf.push_str(&format!("{name}{suffix}\n"));

        ancestor_is_last.push(is_last[i]);
    }

    buf
}

/// Independent reference renderer, kept as a cross-check that `render_tree` is
/// byte-identical against. Deliberately uses a different technique — a bounded
/// forward scan per entry instead of `render_tree`'s monotonic-stack passes —
/// so the two implementations can't share a bug by construction.
///
/// For "is entry `i` the last child", scan forward from `i + 1` and stop at the
/// FIRST entry shallower than `i` (that means we've left `i`'s parent's child
/// list entirely — nothing beyond it is relevant). If a same-depth entry with
/// the same parent path shows up before that, it's a later sibling and `i` is
/// not last. Entries deeper than `i` (its own descendants) never match either
/// condition, so they're correctly skipped over rather than mistaken for
/// siblings — that conflation was the original bug.
#[cfg(test)]
fn render_tree_naive(root_label: &str, entries: &[Collected]) -> String {
    let mut buf = format!("{}/\n", root_label);
    if entries.is_empty() {
        return buf;
    }

    // Is the entry at `idx` (depth `at_depth`, with `at_depth - 1` leading
    // components identifying its parent) the last child of its parent?
    let is_last_child = |idx: usize, at_depth: usize| -> bool {
        for entry in &entries[idx + 1..] {
            let d = entry.components.len();
            if d < at_depth {
                return true; // left the parent's child list: no sibling followed
            }
            if d == at_depth
                && entry.components[..at_depth - 1] == entries[idx].components[..at_depth - 1]
            {
                return false; // later sibling
            }
            // d > at_depth (a descendant of some later-or-equal node) or a
            // same-depth entry under a different parent: keep scanning.
        }
        true // ran off the end without finding a sibling
    };

    for (i, entry) in entries.iter().enumerate() {
        let depth = entry.components.len();

        for level in 0..depth.saturating_sub(1) {
            // The ancestor owning this column sits at depth `level + 1`; is
            // *it* the last child of *its* parent?
            if is_last_child(i, level + 1) {
                buf.push_str("    ");
            } else {
                buf.push_str("│   ");
            }
        }

        if is_last_child(i, depth) {
            buf.push_str("└── ");
        } else {
            buf.push_str("├── ");
        }

        let name = &entry.components[depth - 1];
        let suffix = if entry.is_symlink {
            "@"
        } else if entry.is_dir {
            "/"
        } else {
            ""
        };
        buf.push_str(&format!("{name}{suffix}\n"));
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Collected` from `"a/b/c"`-style paths with a trailing marker:
    /// `/` = dir, `@` = symlink, none = file.
    fn collected(spec: &str) -> Collected {
        let (path, is_dir, is_symlink) = if let Some(p) = spec.strip_suffix('/') {
            (p, true, false)
        } else if let Some(p) = spec.strip_suffix('@') {
            (p, false, true)
        } else {
            (spec, false, false)
        };
        Collected {
            components: path.split('/').map(str::to_string).collect(),
            is_dir,
            is_symlink,
        }
    }

    /// A non-trivial nested structure exercising multiple sibling groups,
    /// several depths, last/non-last children at each depth, and a symlink.
    fn sample_entries() -> Vec<Collected> {
        // DFS pre-order, mirroring `build_sorted`'s dirs-first, then alpha order.
        [
            "src/",
            "src/net/",
            "src/net/http.rs",
            "src/net/tcp.rs",
            "src/main.rs",
            "src/util.rs",
            "tests/",
            "tests/fixtures/",
            "tests/fixtures/data.json",
            "tests/integration.rs",
            "Cargo.toml",
            "link.rs@",
            "README.md",
        ]
        .iter()
        .map(|s| collected(s))
        .collect()
    }

    /// The two independently-implemented renderers must be byte-for-byte
    /// identical — a mechanical cross-check between `render_tree`'s O(n)
    /// monotonic-stack pass and `render_tree_naive`'s bounded forward-scan
    /// approach. Preferred over a hand-written golden (which could silently
    /// encode a bug): every drawn byte is checked against a second, differently
    /// derived implementation over a realistic tree plus edge cases — including
    /// the last-child-directory-with-children and non-last-directory-keeps-bar
    /// cases that the original (shared) bug in both functions used to hide.
    #[test]
    fn render_tree_matches_naive_reference() {
        let entries = sample_entries();
        let fast = render_tree("root", &entries);
        let naive = render_tree_naive("root", &entries);
        assert_eq!(fast, naive, "linear render diverged from reference");

        // Empty, single-entry, and a deep single chain (exercises every prefix
        // column at successive depths) all agree too.
        assert_eq!(render_tree("root", &[]), render_tree_naive("root", &[]));
        let one = vec![collected("only.rs")];
        assert_eq!(render_tree("root", &one), render_tree_naive("root", &one));
        let chain: Vec<Collected> = ["a/", "a/b/", "a/b/c/", "a/b/c/d.rs"]
            .iter()
            .map(|s| collected(s))
            .collect();
        assert_eq!(
            render_tree("root", &chain),
            render_tree_naive("root", &chain)
        );

        // Bug case A: a LAST top-level directory that itself has children.
        let last_dir_with_children: Vec<Collected> = ["a.txt", "d/", "d/c.txt"]
            .iter()
            .map(|s| collected(s))
            .collect();
        assert_eq!(
            render_tree("root", &last_dir_with_children),
            render_tree_naive("root", &last_dir_with_children)
        );

        // Bug case B: a NON-last directory whose children must keep the `│`
        // continuation bar.
        let non_last_dir_with_children: Vec<Collected> = ["d/", "d/c.txt", "z.txt"]
            .iter()
            .map(|s| collected(s))
            .collect();
        assert_eq!(
            render_tree("root", &non_last_dir_with_children),
            render_tree_naive("root", &non_last_dir_with_children)
        );

        // Deep (3-level) cases combining both, once with a trailing sibling
        // (non-last `a/`) and once without (last `a/`).
        let deep_non_last: Vec<Collected> = ["a/", "a/b/", "a/b/c.txt", "z.txt"]
            .iter()
            .map(|s| collected(s))
            .collect();
        assert_eq!(
            render_tree("root", &deep_non_last),
            render_tree_naive("root", &deep_non_last)
        );
        let deep_last: Vec<Collected> = ["a/", "a/b/", "a/b/c.txt"]
            .iter()
            .map(|s| collected(s))
            .collect();
        assert_eq!(
            render_tree("root", &deep_last),
            render_tree_naive("root", &deep_last)
        );
    }

    /// Bug case A: a directory that is the LAST entry at its level but still
    /// has children must draw `└── `, not `├── ` — and its child's
    /// continuation column for that directory's level must be blank, not `│`.
    #[test]
    fn render_tree_last_child_dir_with_children() {
        let entries: Vec<Collected> = ["a.txt", "d/", "d/c.txt"]
            .iter()
            .map(|s| collected(s))
            .collect();
        let out = render_tree("root", &entries);

        assert!(
            out.contains("└── d/"),
            "last-child dir with children needs └──: {out}"
        );
        assert!(
            !out.contains("├── d/"),
            "last-child dir with children must not use ├──: {out}"
        );
        assert!(
            out.contains("    └── c.txt"),
            "d's child must have a blank continuation column: {out}"
        );
        assert!(
            !out.contains("│   └── c.txt"),
            "d's child must not carry a │ bar (d has no later sibling): {out}"
        );
    }

    /// Bug case B: a NON-last directory's children must keep the `│`
    /// continuation bar all the way down, even for the directory's own last
    /// child.
    #[test]
    fn render_tree_non_last_dir_keeps_bar() {
        let entries: Vec<Collected> = ["d/", "d/c.txt", "z.txt"]
            .iter()
            .map(|s| collected(s))
            .collect();
        let out = render_tree("root", &entries);

        assert!(out.contains("├── d/"), "non-last dir needs ├──: {out}");
        assert!(
            out.contains("│   └── c.txt"),
            "non-last dir's child must keep the │ continuation bar: {out}"
        );
        assert!(
            out.contains("└── z.txt"),
            "trailing sibling needs └──: {out}"
        );
    }

    /// A 3-level-deep tree exercising both bug cases together, so each
    /// ancestor's prefix column is checked independently at depth 3.
    #[test]
    fn render_tree_deep_ancestor_columns() {
        // `a/` is NOT last (z.txt follows) — every column tracing through `a`
        // must carry a `│` bar, while `b` (a's only, thus last, child) still
        // contributes a blank column for depths below it.
        let non_last: Vec<Collected> = ["a/", "a/b/", "a/b/c.txt", "z.txt"]
            .iter()
            .map(|s| collected(s))
            .collect();
        let out = render_tree("root", &non_last);
        assert!(out.contains("├── a/"), "non-last a/: {out}");
        assert!(out.contains("│   └── b/"), "b under non-last a/: {out}");
        assert!(
            out.contains("│       └── c.txt"),
            "c.txt: bar from a/, blank from (last) b/: {out}"
        );
        assert!(out.contains("└── z.txt"), "trailing sibling: {out}");

        // `a/` IS last (no trailing sibling) — every column collapses to
        // blank all the way down.
        let last: Vec<Collected> = ["a/", "a/b/", "a/b/c.txt"]
            .iter()
            .map(|s| collected(s))
            .collect();
        let out = render_tree("root", &last);
        assert!(out.contains("└── a/"), "last a/: {out}");
        assert!(out.contains("    └── b/"), "b under last a/: {out}");
        assert!(
            out.contains("        └── c.txt"),
            "c.txt: blank from both last a/ and last b/: {out}"
        );
    }

    #[tokio::test]
    async fn tree_basic_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/integration.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        let c = ToolContext::new(dir.path().to_path_buf());
        let out = TreeTool
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap(), "max_depth": 2}),
                &c,
            )
            .await
            .unwrap();

        // Root dir shown with trailing slash.
        assert!(out.contains("/\n"), "root dir marker: {out}");
        // Dirs shown with trailing slash.
        assert!(out.contains("src/"), "src dir: {out}");
        assert!(out.contains("tests/"), "tests dir: {out}");
        // Files shown without trailing slash.
        assert!(out.contains("main.rs"), "main.rs: {out}");
        assert!(out.contains("integration.rs"), "integration.rs: {out}");
        assert!(out.contains("README.md"), "README.md: {out}");
        assert!(out.contains("Cargo.toml"), "Cargo.toml: {out}");
        // Box-drawing characters present.
        assert!(
            out.contains("├── ") || out.contains("└── "),
            "connectors: {out}"
        );
    }

    #[tokio::test]
    async fn tree_allows_a_root_outside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("a.txt"), "").unwrap();

        let ctx = ToolContext::new(cwd.path().to_path_buf());
        let out = TreeTool
            .execute(
                serde_json::json!({"path": outside.path().to_str().unwrap()}),
                &ctx,
            )
            .await
            .expect("walking a tree outside cwd is allowed");
        assert!(out.contains("a.txt"), "got: {out}");
    }

    /// Hidden dotdirs are hidden by default and only shown when `hidden:
    /// true` is set — the undocumented default this change documents and
    /// makes overridable.
    #[tokio::test]
    async fn tree_hides_dotdir_by_default_and_shows_it_with_hidden_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".dotdir")).unwrap();
        std::fs::write(dir.path().join(".dotdir/inside.txt"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let c = ToolContext::new(dir.path().to_path_buf());
        let out = TreeTool
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap(), "max_depth": 2}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("visible.txt"), "{out}");
        assert!(!out.contains(".dotdir"), "{out}");

        let out = TreeTool
            .execute(
                serde_json::json!({
                    "path": dir.path().to_str().unwrap(),
                    "max_depth": 2,
                    "hidden": true
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains(".dotdir/"), "{out}");
        assert!(out.contains("inside.txt"), "{out}");
    }

    #[tokio::test]
    async fn tree_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        // Minimal .git dir so ignore crate recognises the repo root.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();

        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/ignored.rs"), "").unwrap();
        std::fs::write(dir.path().join("src.rs"), "").unwrap();

        let c = ToolContext::new(dir.path().to_path_buf());
        let out = TreeTool
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap(), "max_depth": 2}),
                &c,
            )
            .await
            .unwrap();

        assert!(out.contains("src.rs"), "normal file: {out}");
        assert!(!out.contains("ignored.rs"), "gitignored file: {out}");
        assert!(!out.contains("target"), "gitignored dir: {out}");
    }

    #[tokio::test]
    async fn tree_max_depth_limits_recursion() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c/d")).unwrap();
        std::fs::write(dir.path().join("a/deep.rs"), "").unwrap();

        let c = ToolContext::new(dir.path().to_path_buf());

        // The first line is the root's absolute path, and tempdir names are
        // random: one ending in `b` would satisfy `contains("b/")` on its own.
        // Assert against the tree body instead.
        let body = |out: &str| out.split_once('\n').map(|(_, b)| b.to_string()).unwrap();

        // depth 1: only shows a/
        let out1 = TreeTool
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap(), "max_depth": 1}),
                &c,
            )
            .await
            .unwrap();
        let out1 = body(&out1);
        assert!(out1.contains("a/"), "depth 1: {out1}");
        assert!(!out1.contains("b/"), "depth 1 must not show b/: {out1}");
        assert!(
            !out1.contains("deep.rs"),
            "depth 1 must not show deep.rs: {out1}"
        );

        // depth 3: shows a/b/c/ but not d/
        let out3 = TreeTool
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap(), "max_depth": 3}),
                &c,
            )
            .await
            .unwrap();
        let out3 = body(&out3);
        assert!(out3.contains("c/"), "depth 3: {out3}");
        assert!(!out3.contains("d/"), "depth 3 must not show d/: {out3}");
    }

    #[tokio::test]
    async fn tree_defaults_to_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "").unwrap();

        let c = ToolContext::new(dir.path().to_path_buf());
        let out = TreeTool
            .execute(serde_json::json!({"max_depth": 1}), &c)
            .await
            .unwrap();

        assert!(out.contains("hello.txt"), "default cwd: {out}");
        // Root shown as "." when no path given.
        assert!(out.starts_with("./\n"), "default root: {out}");
    }

    #[tokio::test]
    async fn tree_symlink_marks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("file.txt"), dir.path().join("link.txt"))
                .unwrap();
            let c = ToolContext::new(dir.path().to_path_buf());
            let out = TreeTool
                .execute(
                    serde_json::json!({"path": dir.path().to_str().unwrap(), "max_depth": 1}),
                    &c,
                )
                .await
                .unwrap();
            assert!(out.contains("link.txt@"), "symlink mark: {out}");
        }
    }

    #[tokio::test]
    async fn tree_empty_directory() {
        let dir = tempfile::tempdir().unwrap();

        let c = ToolContext::new(dir.path().to_path_buf());
        let out = TreeTool
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap(), "max_depth": 1}),
                &c,
            )
            .await
            .unwrap();

        // Should just show the root directory with trailing slash and nothing else.
        let lines: Vec<&str> = out.lines().collect();
        // Root dir line only, no child entries.
        assert!(lines.len() <= 2, "empty dir should be minimal: {out}");
        assert!(out.contains('/'), "root dir marker: {out}");
    }

    #[tokio::test]
    async fn tree_nested_proper_connectors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/x.rs"), "").unwrap();
        std::fs::write(dir.path().join("a/y.rs"), "").unwrap();
        std::fs::write(dir.path().join("z.txt"), "").unwrap();

        let c = ToolContext::new(dir.path().to_path_buf());
        let out = TreeTool
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap(), "max_depth": 2}),
                &c,
            )
            .await
            .unwrap();

        // z.txt (last top-level entry) should use └──
        assert!(
            out.contains("└── z.txt"),
            "last top entry needs └── connector: {out}"
        );
        // a/ (first top-level, not last) should use ├──
        assert!(
            out.contains("├── a/"),
            "non-last dir needs ├── connector: {out}"
        );
        // x.rs inside a/ (first child, not last) should have continuation prefix
        assert!(
            out.contains("│   ├── x.rs"),
            "nested non-last needs │: {out}"
        );
        // y.rs inside a/ (a's own last child) must still carry the │ bar for
        // a's column, because a/ itself is NOT last (z.txt follows it) — the
        // column reflects a's last-child status, not y.rs's.
        assert!(
            out.contains("│   └── y.rs"),
            "nested last child under a non-last dir keeps │: {out}"
        );
    }
}
