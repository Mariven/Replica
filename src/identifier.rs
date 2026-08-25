//! An identifier is the relative path that makes files at different refs count as the same file.
//! We store it as path components: `docs`, `style`, `main.md`.

use anyhow::Result;
use globset::{GlobBuilder, GlobMatcher};

/// A relative path, split on `/`.
pub type Identifier = Vec<String>;

/// Join `["docs", "style"]` into `docs/style`.
pub fn identifier_to_string(ident: &Identifier) -> String {
    ident.join("/")
}

/// Split `docs/style` into `["docs", "style"]`.
pub fn identifier_from_string(s: &str) -> Identifier {
    s.split('/').map(str::to_owned).collect()
}

/// Files this relation includes. A glob is shell-style: `*` stops at `/`.
pub enum Pattern {
    All,
    Glob(GlobMatcher),
}

impl Pattern {
    /// Compile a glob where `*` matches a single path component.
    pub fn glob(pat: &str) -> Result<Pattern> {
        let matcher = GlobBuilder::new(pat).literal_separator(true).build()?.compile_matcher();
        Ok(Pattern::Glob(matcher))
    }

    /// True if this file is included (`All`, or the glob matches).
    pub fn matches(&self, ident: &Identifier) -> bool {
        match self {
            Pattern::All => true,
            Pattern::Glob(m) => m.is_match(identifier_to_string(ident)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_does_not_cross_separators() {
        let p = Pattern::glob("*.js").unwrap();
        assert!(p.matches(&identifier_from_string("app.js")));
        assert!(!p.matches(&identifier_from_string("sub/app.js")));

        let p = Pattern::glob("docs/**").unwrap();
        assert!(p.matches(&identifier_from_string("docs/style/main.md")));
        assert!(!p.matches(&identifier_from_string("src/main.rs")));
    }
}
