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
        "Show a directory tree (respects .gitignore). Directories get a trailing `/`, \
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
        let entries = collect_entries(&root, depth, max_entries)?;

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

/// Walk `root` with `ignore::WalkBuilder` (honours `.gitignore`/`.ignore`),
/// returning a sorted list of entries capped per-directory at `max_entries`.
fn collect_entries(root: &Path, max_depth: usize, max_entries: usize) -> Result<Vec<Collected>> {
    if max_depth == 0 {
        return Ok(Vec::new());
    }

    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(max_depth))
        .hidden(true)
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
fn render_tree(root_label: &str, entries: &[Collected]) -> String {
    let mut buf = format!("{}/\n", root_label);
    if entries.is_empty() {
        return buf;
    }

    // Group entries by depth. We track which entries are the last child of their
    // parent so we can draw connectors correctly.
    let n = entries.len();
    for i in 0..n {
        let depth = entries[i].components.len();

        // Determine whether this entry is the last child of its parent
        // by looking ahead: if the next entry at the same depth or shallower
        // has a different prefix at depth-1, this is the last child.
        let is_last = (i + 1..n).all(|j| {
            entries[j].components.len() < depth
                || entries[j].components[..depth - 1] != entries[i].components[..depth - 1]
        });

        // Build the prefix: for each ancestor level, emit "│   " or "    "
        // depending on whether that ancestor was the last child.
        for level in 0..depth.saturating_sub(1) {
            let ancestor_last = (i + 1..n).all(|j| {
                entries[j].components.len() <= level
                    || entries[j].components[..=level] != entries[i].components[..=level]
            });
            if ancestor_last {
                buf.push_str("    ");
            } else {
                buf.push_str("│   ");
            }
        }

        // Connector.
        if is_last {
            buf.push_str("└── ");
        } else {
            buf.push_str("├── ");
        }

        let name = &entries[i].components[depth - 1];
        let suffix = if entries[i].is_symlink {
            "@"
        } else if entries[i].is_dir {
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
        // y.rs inside a/ (last child) should have blank prefix from parent
        assert!(out.contains("    └── y.rs"), "nested last child: {out}");
    }
}
