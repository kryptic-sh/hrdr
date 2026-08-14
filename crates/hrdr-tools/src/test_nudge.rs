//! The post-edit test nudge: one line on the tool result when a session changes
//! executable code and never changes a test.
//!
//! The rule this enforces is already in the write-capable prompt — *"when you
//! fix a bug, add or extend a test that fails on the old code and passes on the
//! fix"*. It was still ignored across a whole session of ten fixes, three of
//! which were wrong in ways a single test would have caught. A rule read once at
//! the top of a context is not present at the moment of the edit; a tool result
//! is. So the reminder rides the result of the call that just broke it.
//!
//! Deliberately quiet. It latches off the moment the model demonstrably writes a
//! test, and it fires at most once per path, because a note that arrives on
//! every edit is one the model learns to skip past.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What the model is told. One line: this rides mutations, so its length is a
/// recurring cost, and the reason has to survive being read in a hurry.
pub const TEST_NUDGE_NOTE: &str = "[test] this change adds no test, and none has been added this session \
     — a fix without a test that fails on the old code is unverified";

/// Session-scoped state behind the nudge, held in [`ToolContext`](crate::ToolContext).
#[derive(Default, Debug)]
pub struct TestNudgeState {
    /// A test has been added somewhere this session. Latches: once the model is
    /// demonstrably writing tests, the nudge has nothing left to say.
    saw_test: bool,
    /// Paths already nudged, so editing one file five times costs one note
    /// rather than five.
    nudged: HashSet<PathBuf>,
}

impl TestNudgeState {
    /// Whether a mutation of `path` could owe a note, cheap enough to ask before
    /// paying for the file read that [`observe`](Self::observe) needs.
    ///
    /// Once the session has seen a test this is always `false`, which is what
    /// keeps the extra read off the common path: a model that writes tests pays
    /// for exactly one.
    pub fn might_nudge(&self, path: &Path) -> bool {
        !self.saw_test && !markers(path).is_empty()
    }

    /// Record a mutation and return the note it owes, if any.
    ///
    /// `before`/`after` are the file's content around the change — not just the
    /// content after. A file that *contains* tests says nothing about whether
    /// this change added one, and in Rust, where unit tests live in a
    /// `#[cfg(test)] mod tests` inside the file they cover, that distinction is
    /// the whole game.
    pub fn observe(&mut self, path: &Path, before: &str, after: &str) -> Option<&'static str> {
        if is_test_path(path) || adds_test(path, before, after) {
            self.saw_test = true;
            return None;
        }
        // Short-circuit order matters: only reach the insert when a note is
        // otherwise owed, so `nudged` records paths actually nudged.
        if self.saw_test || markers(path).is_empty() || !self.nudged.insert(path.to_path_buf()) {
            return None;
        }
        Some(TEST_NUDGE_NOTE)
    }
}

/// Whether the path is itself a test, by the conventions ecosystems actually
/// use. A path we don't recognise falls through to the content check rather
/// than being assumed either way.
fn is_test_path(path: &Path) -> bool {
    let full = path.to_string_lossy().replace('\\', "/");
    let name = full.rsplit('/').next().unwrap_or_default();
    full.contains("/tests/")
        || full.contains("/test/")
        || full.starts_with("tests/")
        || full.starts_with("test/")
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains("_spec.")
        || name.contains(".spec.")
}

/// The strings that mean "a test" in `path`'s language.
///
/// An empty slice is also the answer to "should this file be nudged about at
/// all": a language whose test idiom we cannot recognise is one where we would
/// be guessing in both directions, so config, markup and data files — which have
/// no markers — are silently out of scope.
///
/// The verification ledger borrows the same answer as its definition of "a
/// source file" (a mutation of one invalidates what the session had verified).
/// Deliberately one table rather than two: two definitions of "code" would drift
/// apart, and the pair of features would disagree about the same edit.
pub(crate) fn markers(path: &Path) -> &'static [&'static str] {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => &["#[test]", "#[tokio::test]", "#[test_case", "#[rstest"],
        "py" => &["def test"],
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => &["it(", "test(", "describe("],
        "go" => &["func Test", "func Benchmark", "func Fuzz"],
        "rb" => &["describe ", "def test_"],
        "java" | "kt" | "kts" | "scala" => &["@Test"],
        "cs" => &["[Test]", "[Fact]", "[Theory]", "[TestMethod]"],
        "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" => &["TEST(", "TEST_F(", "TEST_CASE("],
        "php" => &["function test"],
        "swift" => &["func test"],
        "ex" | "exs" => &["test \""],
        _ => &[],
    }
}

/// Whether the change introduced a test, by counting markers on each side. A
/// change that only moves or reformats existing tests correctly reports `false`
/// — the count is unchanged, and no new test was written.
fn adds_test(path: &Path, before: &str, after: &str) -> bool {
    markers(path)
        .iter()
        .any(|m| after.matches(m).count() > before.matches(m).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn a_source_change_with_no_test_anywhere_is_nudged_once_per_file() {
        let mut st = TestNudgeState::default();
        let src = p("crates/x/src/lib.rs");
        assert_eq!(
            st.observe(&src, "fn a() {}", "fn a() { b(); }"),
            Some(TEST_NUDGE_NOTE)
        );
        // Same file again: already said, and repeating it is how a note becomes
        // wallpaper.
        assert_eq!(st.observe(&src, "fn a() { b(); }", "fn a() { c(); }"), None);
        // A different file still owes its own note.
        assert!(
            st.observe(&p("crates/x/src/other.rs"), "", "fn z() {}")
                .is_some()
        );
    }

    #[test]
    fn adding_a_rust_unit_test_inside_the_edited_file_silences_the_session() {
        let mut st = TestNudgeState::default();
        let src = p("crates/x/src/auth.rs");
        // The case that defeats path-based detection: the file is not a test
        // file, and it already contains tests. Only the count moving proves a
        // test was added.
        let before = "fn f() {}\n#[cfg(test)]\nmod tests {\n#[test]\nfn one() {}\n}";
        let after =
            "fn f() {}\n#[cfg(test)]\nmod tests {\n#[test]\nfn one() {}\n#[test]\nfn two() {}\n}";
        assert_eq!(st.observe(&src, before, after), None);
        // Latched: a later source-only change anywhere is no longer nudged.
        assert_eq!(
            st.observe(&p("crates/x/src/net.rs"), "fn g() {}", "fn g() { h(); }"),
            None
        );
    }

    #[test]
    fn a_file_that_merely_contains_tests_does_not_count_as_adding_one() {
        let mut st = TestNudgeState::default();
        let body = "#[cfg(test)]\nmod tests {\n#[test]\nfn one() {}\n}";
        let before = format!("fn f() {{}}\n{body}");
        let after = format!("fn f() {{ changed(); }}\n{body}");
        assert_eq!(
            st.observe(&p("crates/x/src/a.rs"), &before, &after),
            Some(TEST_NUDGE_NOTE),
        );
    }

    #[test]
    fn editing_a_test_file_silences_the_session_whatever_its_content() {
        let mut st = TestNudgeState::default();
        assert_eq!(
            st.observe(&p("crates/x/tests/wire.rs"), "", "fn helper() {}"),
            None
        );
        assert_eq!(
            st.observe(&p("crates/x/src/a.rs"), "fn f() {}", "fn f() { g(); }"),
            None
        );
    }

    #[test]
    fn languages_without_a_known_test_idiom_are_out_of_scope() {
        let mut st = TestNudgeState::default();
        for path in ["Cargo.toml", "README.md", "config.yaml", "data.json"] {
            assert_eq!(
                st.observe(&p(path), "a", "b"),
                None,
                "{path} should not nudge"
            );
            assert!(!st.might_nudge(&p(path)), "{path} should not cost a read");
        }
        // …and the gate is not simply always-false.
        assert!(st.might_nudge(&p("src/a.rs")));
    }

    #[test]
    fn moving_existing_tests_around_is_not_adding_one() {
        let mut st = TestNudgeState::default();
        let before = "#[test]\nfn a() {}\n#[test]\nfn b() {}";
        let after = "#[test]\nfn b() {}\n#[test]\nfn a() {}";
        // Count unchanged, so this is a source change like any other.
        assert_eq!(
            st.observe(&p("src/x.rs"), before, after),
            Some(TEST_NUDGE_NOTE)
        );
    }

    #[test]
    fn each_ecosystem_marker_is_recognised() {
        // One case per language, so a marker table edited by someone who does
        // not use that language still has to keep it working.
        let cases: &[(&str, &str, &str)] = &[
            ("a.py", "", "def test_x():\n    pass"),
            ("a.ts", "", "it('works', () => {})"),
            ("a.go", "", "func TestX(t *testing.T) {}"),
            ("a.rb", "", "def test_x\nend"),
            ("A.java", "", "@Test\nvoid x() {}"),
            ("A.cs", "", "[Fact]\nvoid X() {}"),
            ("a.cpp", "", "TEST(Suite, Case) {}"),
            ("a.php", "", "function testX() {}"),
            ("a.swift", "", "func testX() {}"),
            ("a.exs", "", "test \"x\" do\nend"),
        ];
        for (name, before, after) in cases {
            let mut st = TestNudgeState::default();
            assert_eq!(
                st.observe(&p(name), before, after),
                None,
                "{name}: adding a test should latch the nudge off",
            );
        }
    }
}
