//! A unique, hidden sibling temp path for a crash-safe write-then-atomic-
//! rename: build the new bytes at `<dir>/.<filename>.<tag>-<pid>-<seq>` next
//! to the real target, fsync/write it, then `rename` it over the target.
//! Same directory keeps the rename intra-filesystem (hence atomic); the
//! dot-prefix keeps it out of normal directory listings; PID plus a
//! process-wide counter keep it unique across both concurrent processes and
//! concurrent calls within one process, so two callers racing to write the
//! same target never collide on one temp name (no random/time API needed —
//! names stay deterministic).
//!
//! One scheme shared by every crate that does this: `hrdr-tools`' file writer
//! and its move/copy staging, `hrdr-llm`'s catalog cache writer, and
//! `hrdr-agent`'s credential-store writer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide counter giving each temp path a unique name (paired with the
/// PID). Shared by every call site so two unrelated writers can never be
/// handed the same sequence number.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique, hidden sibling path for `target`: `<dir>/.<filename>.<tag>-<pid>-<seq>`,
/// living in `target`'s parent directory so a subsequent `rename` onto
/// `target` stays on one filesystem. `tag` labels the caller/use-case (e.g.
/// `hrdr-tmp`, `hrdr-stage`, `hrdr-aside`) so a stray leftover is
/// recognizable in a directory listing.
pub fn unique_sibling_path(target: &Path, tag: &str) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".{name}.{tag}-{}-{seq}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two calls for the same target must never collide, must stay hidden
    /// (dot-prefixed), must sit beside the target, and must carry the tag —
    /// the properties every call site relies on.
    #[test]
    fn unique_sibling_path_is_unique_hidden_and_colocated() {
        let target = Path::new("/some/dir/models.json");
        let a = unique_sibling_path(target, "hrdr-tmp");
        let b = unique_sibling_path(target, "hrdr-tmp");

        assert_ne!(a, b, "concurrent callers must not share a temp name");
        assert_eq!(a.parent(), target.parent());
        assert_eq!(b.parent(), target.parent());

        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with('.'), "sibling temp must be hidden: {name}");
            assert!(name.contains("hrdr-tmp"), "tag must appear in name: {name}");
            assert!(
                name.contains(&std::process::id().to_string()),
                "pid must appear in name: {name}"
            );
        }
    }

    /// Different tags for the same target still produce distinct, correctly
    /// labeled names — the `staging_path`/`aside_path` two-tags-one-target
    /// case in `hrdr-tools`' copy staging.
    #[test]
    fn unique_sibling_path_respects_the_tag() {
        let target = Path::new("/proj/dest.txt");
        let stage = unique_sibling_path(target, "hrdr-stage");
        let aside = unique_sibling_path(target, "hrdr-aside");

        assert_ne!(stage, aside);
        assert!(
            stage
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("hrdr-stage")
        );
        assert!(
            aside
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("hrdr-aside")
        );
    }
}
