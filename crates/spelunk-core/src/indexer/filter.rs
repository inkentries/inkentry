//! Index-time file filter: skips generated, vendored, minified, and
//! machine-data files that are committed to the repo (so `.gitignore` never
//! catches them) yet carry near-zero retrieval value while costing real
//! embed/parse wall-clock.
//!
//! This is a **separate layer** from the unconditional sensitive-file exclusion
//! (`.env`, `*.pem`, private keys) applied by the walker's `OverrideBuilder` in
//! `collect_files`. That layer is not user-overridable; nothing here can
//! re-include a sensitive file, because sensitive files are dropped by the walk
//! before this filter ever sees them.
//!
//! Matching uses gitignore syntax via `ignore::gitignore`: built-in defaults are
//! added first, user lines second, and matching is last-match-wins, so a user
//! `!pattern` line re-includes a path the defaults would drop.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Built-in exclude globs (gitignore syntax). Applied first; user lines layer on
/// top with last-match-wins semantics, so `!glob` in user config re-includes.
///
/// Covers package lockfiles, minified assets, common vendored/generated
/// directories, protobuf/codegen outputs, and bulk machine-data (schemas,
/// translation/locale JSON).
pub const DEFAULT_EXCLUDES: &[&str] = &[
    // package lockfiles (machine-written, huge, no recall value)
    "package-lock.json",
    "npm-shrinkwrap.json",
    "packages.lock.json",
    // minified assets
    "*.min.js",
    "*.min.css",
    // vendored / generated directories
    "vendor/",
    "node_modules/",
    "third_party/",
    "dist/",
    "generated/",
    "__generated__/",
    // generated file markers by name
    "*.generated.*",
    "*.gen.go",
    "*.gen.ts",
    "zz_generated*.go",
    // protobuf / grpc codegen
    "*.pb.go",
    "*.pb.cc",
    "*.pb.h",
    "*_pb2.py",
    "*_pb2_grpc.py",
    "*_pb.js",
    "*_pb.d.ts",
    // bulk machine-data
    "schema.json",
    "*.schema.json",
    "**/translations/**/*.json",
    "**/locales/**/*.json",
    "**/locale/**/*.json",
    "**/i18n/**/*.json",
];

/// Sentinel `from` paths recorded on each glob so a match can report whether it
/// came from the built-in defaults or from user config.
const SRC_DEFAULT: &str = "<default>";
const SRC_USER: &str = "<user>";

/// Max bytes read from the head of a file when sniffing for a generated marker.
const MARKER_HEAD_BYTES: usize = 4 * 1024;
/// Number of leading lines a generated marker must appear within.
const MARKER_MAX_LINES: usize = 5;

/// Which glob matched, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchInfo {
    /// The glob as written (e.g. `node_modules/` or `!keep.min.js`).
    pub pattern: String,
    /// True if the glob is a built-in default, false if it came from user config.
    pub from_default: bool,
}

/// Outcome of testing one path against the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No exclude matched (or only the sensitive layer, handled elsewhere): keep.
    Keep,
    /// An exclude glob matched: drop the path. Carries the matched glob.
    Exclude(MatchInfo),
    /// A user `!` line re-included the path: keep it AND exempt it from
    /// generated-marker detection (the user asked for it explicitly).
    ForceInclude(MatchInfo),
}

/// Compiled index filter: the layered gitignore matcher plus the
/// generated-marker toggle.
#[derive(Debug, Clone)]
pub struct IndexFilter {
    gi: Gitignore,
    detect_generated: bool,
}

impl IndexFilter {
    /// Build a filter from user excludes and the two toggles.
    ///
    /// `use_default_excludes` prepends [`DEFAULT_EXCLUDES`]; user lines are added
    /// after so they win on ties. `detect_generated` gates the `@generated` /
    /// `Code generated ... DO NOT EDIT.` header sniff.
    pub fn build(
        user_excludes: &[String],
        use_default_excludes: bool,
        detect_generated: bool,
    ) -> anyhow::Result<Self> {
        // Root "" so paths are matched exactly as passed (already project-relative).
        let mut b = GitignoreBuilder::new("");
        if use_default_excludes {
            let from = Some(PathBuf::from(SRC_DEFAULT));
            for line in DEFAULT_EXCLUDES {
                b.add_line(from.clone(), line)?;
            }
        }
        let from = Some(PathBuf::from(SRC_USER));
        for line in user_excludes {
            b.add_line(from.clone(), line)?;
        }
        let gi = b.build()?;
        Ok(Self {
            gi,
            detect_generated,
        })
    }

    /// Whether generated-marker sniffing is enabled.
    pub fn detect_generated(&self) -> bool {
        self.detect_generated
    }

    /// Classify a project-relative path against the path itself only (no
    /// ancestor lookup, no file read).
    ///
    /// This is the hot-loop entry: during the walk, excluded ancestor
    /// directories are already pruned by [`IndexFilter::prune_dir`], so a plain
    /// per-path match is both correct and cheap. Use [`IndexFilter::classify`]
    /// when the caller has no walk hierarchy (e.g. `spelunk chunks <path>`).
    pub fn decide(&self, rel_path: &Path, is_dir: bool) -> Decision {
        Self::from_match(self.gi.matched(rel_path, is_dir))
    }

    /// Classify a project-relative path, also matching against any excluded
    /// parent directory (e.g. a file under `node_modules/`). More expensive than
    /// [`IndexFilter::decide`]; use it when there is no walk to prune ancestors,
    /// such as explaining why `spelunk chunks <path>` found nothing.
    pub fn classify(&self, rel_path: &Path, is_dir: bool) -> Decision {
        Self::from_match(self.gi.matched_path_or_any_parents(rel_path, is_dir))
    }

    fn from_match(m: ignore::Match<&ignore::gitignore::Glob>) -> Decision {
        match m {
            ignore::Match::None => Decision::Keep,
            ignore::Match::Ignore(glob) => Decision::Exclude(Self::info(glob)),
            ignore::Match::Whitelist(glob) => Decision::ForceInclude(Self::info(glob)),
        }
    }

    /// True if this directory should be pruned from the walk (an exclude glob
    /// matched it). A user `!` re-include of the directory keeps it. Note that
    /// gitignore semantics do not let a `!file` line re-include through an
    /// already-excluded parent directory, matching git itself.
    pub fn prune_dir(&self, rel_path: &Path) -> bool {
        matches!(self.decide(rel_path, true), Decision::Exclude(_))
    }

    fn info(glob: &ignore::gitignore::Glob) -> MatchInfo {
        let from_default = glob
            .from()
            .map(|p| p == Path::new(SRC_DEFAULT))
            .unwrap_or(false);
        MatchInfo {
            pattern: glob.original().to_string(),
            from_default,
        }
    }
}

/// Regex for the Go-style `// Code generated by <tool>. DO NOT EDIT.` header.
/// Compiled once. Applied per line (anchored), not across the whole buffer.
fn marker_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^// Code generated by .* DO NOT EDIT\.$").expect("valid marker regex")
    })
}

/// Return the self-declared generated marker in a file's head, if any.
///
/// Reads at most [`MARKER_HEAD_BYTES`] and inspects the first
/// [`MARKER_MAX_LINES`] lines for a literal `@generated` token or the Go
/// `Code generated ... DO NOT EDIT.` header. Self-declaration only: no
/// line-length, entropy, or statistical heuristics. Returns the marker text for
/// debug logging, or `None`.
pub fn generated_marker(path: &Path) -> Option<&'static str> {
    let head = read_head(path)?;
    marker_in_head(&head)
}

/// Marker scan over an already-read head string (unit-testable core).
fn marker_in_head(head: &str) -> Option<&'static str> {
    for line in head.lines().take(MARKER_MAX_LINES) {
        if line.contains("@generated") {
            return Some("@generated");
        }
        if marker_regex().is_match(line.trim_end()) {
            return Some("Code generated ... DO NOT EDIT.");
        }
    }
    None
}

/// Read up to [`MARKER_HEAD_BYTES`] from the file, lossily decoded so a binary
/// or non-UTF-8 head never errors the scan.
fn read_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; MARKER_HEAD_BYTES];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_filter() -> IndexFilter {
        IndexFilter::build(&[], true, true).unwrap()
    }

    fn is_excluded(f: &IndexFilter, rel: &str, is_dir: bool) -> bool {
        matches!(f.decide(Path::new(rel), is_dir), Decision::Exclude(_))
    }

    #[test]
    fn each_default_class_is_excluded() {
        let f = default_filter();
        // lockfiles
        assert!(is_excluded(&f, "package-lock.json", false));
        assert!(is_excluded(&f, "npm-shrinkwrap.json", false));
        assert!(is_excluded(&f, "packages.lock.json", false));
        // minified
        assert!(is_excluded(&f, "app.min.js", false));
        assert!(is_excluded(&f, "site.min.css", false));
        // vendored / generated directories (as dirs)
        assert!(is_excluded(&f, "vendor", true));
        assert!(is_excluded(&f, "node_modules", true));
        assert!(is_excluded(&f, "third_party", true));
        assert!(is_excluded(&f, "dist", true));
        assert!(is_excluded(&f, "generated", true));
        assert!(is_excluded(&f, "__generated__", true));
        // A file nested under a vendored dir: `decide` matches the path itself
        // only (ancestors are pruned during the walk), so the parent-aware
        // `classify` is what recognises it out of walk context.
        assert!(matches!(
            f.classify(Path::new("node_modules/react/index.js"), false),
            Decision::Exclude(_)
        ));
        // generated file markers
        assert!(is_excluded(&f, "api.generated.ts", false));
        assert!(is_excluded(&f, "types.gen.go", false));
        assert!(is_excluded(&f, "client.gen.ts", false));
        assert!(is_excluded(&f, "zz_generated_deepcopy.go", false));
        // protobuf / grpc
        assert!(is_excluded(&f, "user.pb.go", false));
        assert!(is_excluded(&f, "user.pb.cc", false));
        assert!(is_excluded(&f, "user.pb.h", false));
        assert!(is_excluded(&f, "user_pb2.py", false));
        assert!(is_excluded(&f, "user_pb2_grpc.py", false));
        assert!(is_excluded(&f, "user_pb.js", false));
        assert!(is_excluded(&f, "user_pb.d.ts", false));
        // machine-data
        assert!(is_excluded(&f, "schema.json", false));
        assert!(is_excluded(&f, "openapi.schema.json", false));
        assert!(is_excluded(&f, "src/translations/en/messages.json", false));
        assert!(is_excluded(&f, "src/locales/en.json", false));
        assert!(is_excluded(&f, "app/locale/fr.json", false));
        assert!(is_excluded(&f, "src/i18n/en.json", false));
    }

    #[test]
    fn survivors_are_kept() {
        let f = default_filter();
        for rel in [
            "src/lib.rs",
            "package.json",
            "tsconfig.json",
            "README.md",
            "tests/foo_test.rs",
            // .ts under i18n/ survives: the i18n default only excludes *.json
            "src/i18n/index.ts",
        ] {
            assert_eq!(
                f.decide(Path::new(rel), false),
                Decision::Keep,
                "{rel} must be kept"
            );
        }
    }

    #[test]
    fn user_bang_line_reincludes() {
        // A user `!` re-include wins over a default (last-match-wins) and yields
        // ForceInclude (exempt from marker detection).
        let f = IndexFilter::build(&["!vendored.min.js".to_string()], true, true).unwrap();
        match f.decide(Path::new("vendored.min.js"), false) {
            Decision::ForceInclude(mi) => {
                assert!(!mi.from_default, "the re-include came from user config");
            }
            other => panic!("expected ForceInclude, got {other:?}"),
        }
        // A different .min.js is still excluded.
        assert!(is_excluded(&f, "other.min.js", false));
    }

    #[test]
    fn use_default_excludes_false_disables_builtins() {
        let f = IndexFilter::build(&[], false, true).unwrap();
        assert_eq!(
            f.decide(Path::new("package-lock.json"), false),
            Decision::Keep
        );
        assert_eq!(f.decide(Path::new("node_modules"), true), Decision::Keep);
    }

    #[test]
    fn user_excludes_apply_without_defaults() {
        // Defaults off, but a user glob still excludes.
        let f = IndexFilter::build(&["*.bin".to_string()], false, true).unwrap();
        match f.decide(Path::new("blob.bin"), false) {
            Decision::Exclude(mi) => assert!(!mi.from_default),
            other => panic!("expected Exclude, got {other:?}"),
        }
    }

    #[test]
    fn match_info_reports_default_source_and_pattern() {
        let f = default_filter();
        match f.decide(Path::new("node_modules"), true) {
            Decision::Exclude(mi) => {
                assert!(mi.from_default);
                assert_eq!(mi.pattern, "node_modules/");
            }
            other => panic!("expected Exclude, got {other:?}"),
        }
    }

    #[test]
    fn generated_marker_fires_within_first_lines_only() {
        // @generated on line 1.
        assert_eq!(
            marker_in_head("// @generated\nfn a() {}\n"),
            Some("@generated")
        );
        // Go header on line 1.
        assert_eq!(
            marker_in_head("// Code generated by protoc. DO NOT EDIT.\npackage x\n"),
            Some("Code generated ... DO NOT EDIT.")
        );
        // @generated inside the window (line 5) still fires.
        let within = "1\n2\n3\n4\n// @generated\n";
        assert_eq!(marker_in_head(within), Some("@generated"));
        // Past the 5-line window: no match.
        let beyond = "1\n2\n3\n4\n5\n// @generated\n";
        assert_eq!(marker_in_head(beyond), None);
        // Ordinary source: no match.
        assert_eq!(marker_in_head("fn main() {}\n"), None);
        // Near-miss Go header (missing trailing period) must not match.
        assert_eq!(
            marker_in_head("// Code generated by protoc. DO NOT EDIT\n"),
            None
        );
    }

    #[test]
    fn generated_marker_reads_file_head() {
        let dir = tempfile::tempdir().unwrap();
        let generated = dir.path().join("g.rs");
        std::fs::write(&generated, "// @generated by tool\nfn a() {}\n").unwrap();
        assert_eq!(generated_marker(&generated), Some("@generated"));

        let plain = dir.path().join("p.rs");
        std::fs::write(&plain, "fn a() {}\n").unwrap();
        assert_eq!(generated_marker(&plain), None);
    }
}
