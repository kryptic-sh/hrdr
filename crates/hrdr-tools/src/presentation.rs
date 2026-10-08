use std::path::Path;

/// Render a path label without changing the filesystem path. On Windows,
/// `unix_style_paths` selects `/` rather than `\`; other platforms are unchanged.
pub fn display_path(path: &Path, unix_style_paths: bool) -> String {
    let label = path.display().to_string();
    if cfg!(windows) {
        if unix_style_paths {
            label.replace('\\', "/")
        } else {
            label.replace('/', "\\")
        }
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EditTool, ReplaceTool, Tool, ToolContext, WriteTool};
    use serde_json::json;

    #[test]
    fn unix_style_paths_labels() {
        for (input, unix, native) in [
            (
                r"C:\work/src\file.rs",
                "C:/work/src/file.rs",
                r"C:\work\src\file.rs",
            ),
            (
                r"\\server\share/file",
                "//server/share/file",
                r"\\server\share\file",
            ),
            (r"\\?\C:\work/file", "//?/C:/work/file", r"\\?\C:\work\file"),
            (
                r"\\?\UNC\server\share/file",
                "//?/UNC/server/share/file",
                r"\\?\UNC\server\share\file",
            ),
            (
                r"src\nested/file.rs",
                "src/nested/file.rs",
                r"src\nested\file.rs",
            ),
            (
                "/tmp/literal\\name",
                "/tmp/literal/name",
                r"\tmp\literal\name",
            ),
        ] {
            for (style, windows_expected) in [(true, unix), (false, native)] {
                let expected = if cfg!(windows) {
                    windows_expected
                } else {
                    input
                };
                assert_eq!(display_path(Path::new(input), style), expected);
            }
        }
    }

    #[tokio::test]
    async fn unix_style_paths_mutation_results() {
        let dir = tempfile::tempdir().unwrap();
        let default = ToolContext::new(dir.path());
        assert!(default.unix_style_paths);
        let mut native = default.clone();
        native.unix_style_paths = false;
        assert!(!native.clone().unix_style_paths);
        assert!(default.unix_style_paths);
        for (index, ctx) in [&default, &native, &default].into_iter().enumerate() {
            // A literal backslash is part of the filename on Unix, not a separator.
            let relative = if cfg!(windows) {
                format!("nested/file-{index}.txt")
            } else {
                format!("nested/literal\\file-{index}.txt")
            };
            let target = dir.path().join(&relative);
            let before = "old /slash \\backslash\n";
            let after = "new /slash \\backslash\n";
            let created = WriteTool
                .execute(json!({"path": relative, "content": before}), ctx)
                .await
                .unwrap();
            let raw = target.display().to_string();
            let label = if cfg!(windows) {
                if ctx.unix_style_paths {
                    raw.replace('\\', "/")
                } else {
                    raw.replace('/', "\\")
                }
            } else {
                raw
            };
            assert_eq!(created, format!("Created {label} (1 lines)"));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
            let edited = EditTool
                .execute(
                    json!({"path": relative, "old_string": "old", "new_string": "new"}),
                    ctx,
                )
                .await
                .unwrap();
            assert!(
                edited.starts_with(&format!("Replaced 1 occurrence(s) in {label}")),
                "{edited}"
            );
            assert_diff(&edited, &label, before, after);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), after);
            let written = WriteTool
                .execute(json!({"path": relative, "content": before}), ctx)
                .await
                .unwrap();
            assert!(
                written.starts_with(&format!("Wrote {} bytes to {label}", before.len())),
                "{written}"
            );
            assert_diff(&written, &label, after, before);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
            let rel = if cfg!(windows) && !ctx.unix_style_paths {
                relative.replace('/', "\\")
            } else {
                relative.to_string()
            };
            for dry_run in [true, false] {
                let replaced = ReplaceTool.execute(json!({"pattern": "old", "replace": "new", "literal": true, "glob": "**/*.txt", "dry_run": dry_run}), ctx).await.unwrap();
                let verb = if dry_run { "Would replace" } else { "Replaced" };
                assert!(
                    replaced.starts_with(&format!("{verb} 1 occurrence across 1 file:\n{rel}\n")),
                    "{replaced}"
                );
                assert_diff(&replaced, &rel, before, after);
                assert_eq!(
                    std::fs::read_to_string(&target).unwrap(),
                    if dry_run { before } else { after }
                );
            }
        }
    }

    fn assert_diff(output: &str, label: &str, before: &str, after: &str) {
        let label = label.trim_start_matches('/');
        assert!(
            output.contains(&format!("--- a/{label}\n+++ b/{label}\n")),
            "{output}"
        );
        assert!(output.contains(&format!("-{before}+{after}")), "{output}");
    }
}
