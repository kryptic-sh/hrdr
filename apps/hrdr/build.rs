use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

const UNKNOWN: &str = "unknown";

fn git(directory: &Path, args: &[&str]) -> Option<Output> {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()
}

fn has_ascii_control(value: &str) -> bool {
    value.bytes().any(|byte| byte.is_ascii_control())
}

fn git_path(output: Output) -> Option<PathBuf> {
    output.status.success().then_some(())?;
    let output = std::str::from_utf8(&output.stdout).ok()?;
    let output = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    (!output.is_empty() && !has_ascii_control(output)).then(|| PathBuf::from(output))
}

fn has_only_normal_components(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn is_clean_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|path| !has_ascii_control(path))
        && path.components().all(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
}

fn workspace_root(manifest_dir: &Path) -> Option<PathBuf> {
    git(manifest_dir, &["rev-parse", "--show-toplevel"])
        .and_then(git_path)
        .and_then(|root| root.canonicalize().ok())
        .filter(|root| root.is_dir() && is_clean_absolute_path(root))
}

pub(crate) fn tracked_path(root: &Path, path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    (!has_ascii_control(path.to_str()?)
        && !path.is_absolute()
        && !path.as_os_str().is_empty()
        && !path.to_string_lossy().contains('\\')
        && has_only_normal_components(path))
    .then(|| root.join(path))
}

pub(crate) fn tracked_files(root: &Path) -> Vec<PathBuf> {
    let Some(output) = git(root, &["ls-files", "-z"]) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    output
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|path| !path.is_empty())
        .filter_map(|path| std::str::from_utf8(path).ok())
        .filter_map(|path| tracked_path(root, path))
        .filter(|path| path.exists())
        .collect()
}

fn git_metadata_dirs(root: &Path) -> Vec<PathBuf> {
    ["--git-dir", "--git-common-dir"]
        .into_iter()
        .filter_map(|arg| {
            git(root, &["rev-parse", "--path-format=absolute", arg]).and_then(git_path)
        })
        .filter(|path| is_clean_absolute_path(path))
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.is_dir() && is_clean_absolute_path(path))
        .collect()
}

fn valid_ref(reference: &str) -> bool {
    !has_ascii_control(reference)
        && reference.starts_with("refs/")
        && !reference.contains('\\')
        && has_only_normal_components(Path::new(reference))
}

fn canonical_metadata_path(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        path.canonicalize().ok()
    } else {
        let parent = path.parent()?.canonicalize().ok()?;
        let name = path.file_name()?;
        Some(parent.join(name))
    }
}

fn git_metadata_path(root: &Path, name: &str) -> Option<PathBuf> {
    let path = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )
    .and_then(git_path)?;
    is_clean_absolute_path(&path).then_some(())?;
    let path = canonical_metadata_path(&path)?;
    is_clean_absolute_path(&path).then_some(())?;
    git_metadata_dirs(root)
        .iter()
        .any(|directory| path.starts_with(directory))
        .then_some(path)
}

fn symbolic_ref(head: &Path) -> Option<String> {
    let head = fs::read_to_string(head).ok()?;
    let reference = head.strip_prefix("ref: ")?.trim_end_matches(['\r', '\n']);
    valid_ref(reference).then(|| reference.to_owned())
}

pub(crate) fn rerun_directive_path(path: &Path) -> Option<&str> {
    let value = path.to_str()?;
    (!value.is_empty() && !has_ascii_control(value)).then_some(value)
}

fn emit_rerun_path(path: &Path) {
    if let Some(path) = rerun_directive_path(path) {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn emit_rerun_metadata(manifest_dir: &Path) -> Option<()> {
    let root = workspace_root(manifest_dir)?;

    // The binary identifies the checkout when it was compiled. Watch the tracked
    // inputs that can change that identity, not the workspace directory's live state.
    for path in tracked_files(&root) {
        emit_rerun_path(&path);
    }

    let head = git_metadata_path(&root, "HEAD");
    let index = git_metadata_path(&root, "index");
    let packed_refs = git_metadata_path(&root, "packed-refs");
    for path in [&head, &index, &packed_refs]
        .iter()
        .filter_map(|path| path.as_ref())
        .filter(|path| path.exists())
    {
        emit_rerun_path(path);
    }

    if let Some(reference) = head.as_deref().and_then(symbolic_ref)
        && let Some(path) = git_metadata_path(&root, &reference)
    {
        // Watch the containing directory so a packed ref becoming a loose ref rebuilds.
        if let Some(parent) = path.parent() {
            emit_rerun_path(parent);
        }
    }

    Some(())
}

fn object_id(output: &Output) -> Option<&str> {
    let commit = std::str::from_utf8(&output.stdout).ok()?.trim();
    ((commit.len() == 40 || commit.len() == 64)
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(commit)
}

fn build_identity(root: Option<&Path>) -> (String, &'static str) {
    let revision = root.and_then(|root| git(root, &["rev-parse", "--verify", "HEAD"]));
    let status = root.and_then(|root| {
        git(
            root,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "--untracked-files=normal",
            ],
        )
    });

    let commit = revision
        .as_ref()
        .and_then(|output| output.status.success().then_some(output))
        .and_then(object_id);
    let source_state = match (&commit, &status) {
        (Some(_), Some(status)) if status.status.success() => {
            let status = std::str::from_utf8(&status.stdout).ok();
            match status {
                Some("") => "clean",
                Some(_) => "dirty",
                None => UNKNOWN,
            }
        }
        _ => UNKNOWN,
    };
    (commit.unwrap_or(UNKNOWN).to_owned(), source_state)
}

fn safe_rustc_env(value: String) -> String {
    if !has_ascii_control(&value) && !value.is_empty() {
        value
    } else {
        UNKNOWN.to_owned()
    }
}

fn main() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    let root = manifest_dir.as_deref().and_then(workspace_root);
    let (commit, source_state) = build_identity(root.as_deref());
    let target = safe_rustc_env(env::var("TARGET").unwrap_or_else(|_| UNKNOWN.to_owned()));

    println!("cargo:rerun-if-env-changed=TARGET");
    if let Some(manifest_dir) = manifest_dir.as_deref() {
        emit_rerun_metadata(manifest_dir);
    }

    println!("cargo:rustc-env=HRDR_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=HRDR_BUILD_TARGET={target}");
    println!("cargo:rustc-env=HRDR_BUILD_SOURCE_STATE={source_state}");
}

#[cfg(test)]
mod tests {
    use super::{rerun_directive_path, tracked_path};
    use std::path::Path;

    #[test]
    fn accepts_only_safe_relative_tracked_paths() {
        let root = Path::new("workspace");
        assert_eq!(
            tracked_path(root, "crates/hrdr/src/lib.rs"),
            Some(root.join("crates/hrdr/src/lib.rs"))
        );
        assert_eq!(tracked_path(root, ""), None);
        assert_eq!(tracked_path(root, "../outside"), None);
        assert_eq!(tracked_path(root, r"C:\outside"), None);
        assert_eq!(tracked_path(root, "newline\nfile"), None);
    }

    #[test]
    fn rerun_directives_reject_control_characters() {
        assert_eq!(
            rerun_directive_path(Path::new("safe/path")),
            Some("safe/path")
        );
        assert_eq!(rerun_directive_path(Path::new("unsafe\rpath")), None);
    }
}
