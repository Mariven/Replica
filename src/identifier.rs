//! Index files across revisions by relative path segments, matching them against glob patterns.

use anyhow::Result;
use globset::{GlobBuilder, GlobMatcher};

/// Relative path decomposed into individual component segments.
pub type Identifier = Vec<String>;

/// Join path segments with slashes into a single relative path string.
pub fn identifier_to_string(ident: &Identifier) -> String {
    ident.join("/")
}

/// Split a slash-delimited path string into individual path segments.
pub fn identifier_from_string(s: &str) -> Identifier {
    s.split('/').map(str::to_owned).collect()
}

/// Match paths unconditionally across all files, or evaluate against a glob pattern with wildcards confined to individual directory segments.
pub enum Pattern {
    All,
    Glob(GlobMatcher),
}

impl Pattern {
    /// Build a pattern matcher with wildcards restricted to individual path segments.
    pub fn glob(pat: &str) -> Result<Pattern> {
        // Treat separators literally to restrict wildcards to individual directory segments.
        let matcher = GlobBuilder::new(pat).literal_separator(true).build()?.compile_matcher();
        Ok(Pattern::Glob(matcher))
    }

    /// Evaluate a path identifier, returning true for all paths or checking against the compiled glob matcher.
    pub fn matches(&self, ident: &Identifier) -> bool {
        match self {
            Pattern::All => true,
            // Reconstruct the slash-delimited path string to test against the glob matcher.
            Pattern::Glob(m) => m.is_match(identifier_to_string(ident)),
        }
    }

    /// Return the deepest directory level a match can occupy, counted in segments below the root, or None when matches can occur at any depth.
    /// With separators literal, `*` and `?` never match `/`, so a glob without `**` matches paths with at most as many separators as it spells out.
    /// A character class is left unbounded: globset lets a negated class or a range such as `[ -~]` match `/`.
    pub fn max_depth(&self) -> Option<usize> {
        match self {
            Pattern::All => None,
            Pattern::Glob(m) => {
                let text = m.glob().glob();
                if text.contains("**") || text.contains('[') { None } else { Some(text.matches('/').count() + 1) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verify single asterisks match within a directory segment while double asterisks traverse directory boundaries.
    #[test]
    fn glob_does_not_cross_separators() {
        // Match single asterisks within the immediate directory segment.
        let p = Pattern::glob("*.js").unwrap();
        assert!(p.matches(&identifier_from_string("app.js")));
        assert!(!p.matches(&identifier_from_string("sub/app.js")));

        // Match double asterisks across nested directory paths.
        let p = Pattern::glob("docs/**").unwrap();
        assert!(p.matches(&identifier_from_string("docs/style/main.md")));
        assert!(!p.matches(&identifier_from_string("src/main.rs")));
    }

    #[test]
    fn max_depth_follows_literal_separators() {
        assert_eq!(Pattern::All.max_depth(), None);
        assert_eq!(Pattern::glob("SOUL.md").unwrap().max_depth(), Some(1));
        assert_eq!(Pattern::glob("*.mdc").unwrap().max_depth(), Some(1));
        assert_eq!(Pattern::glob("docs/*.md").unwrap().max_depth(), Some(2));
        assert_eq!(Pattern::glob("{docs/a,b}.md").unwrap().max_depth(), Some(2));
        assert_eq!(Pattern::glob("docs/**").unwrap().max_depth(), None);
        assert_eq!(Pattern::glob("**/*.md").unwrap().max_depth(), None);
        // globset lets a negated class match a separator: a[!z]b.md matches a/b.md.
        assert!(Pattern::glob("a[!z]b.md").unwrap().matches(&identifier_from_string("a/b.md")));
        assert_eq!(Pattern::glob("a[!z]b.md").unwrap().max_depth(), None);
        assert_eq!(Pattern::glob("notes[0-9].md").unwrap().max_depth(), None);
    }
}
