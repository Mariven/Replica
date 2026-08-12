use anyhow::Result;
use globset::{GlobBuilder, GlobMatcher};

// An Identifier is the shared name under which files at different refs count as the same file: a normalized relative path, stored as components. Components never contain '/'.
pub type Identifier = Vec<String>;

pub fn identifier_to_string(ident: &Identifier) -> String {
    ident.join("/")
}

pub fn identifier_from_string(s: &str) -> Identifier {
    s.split('/').map(str::to_owned).collect()
}

// Which identifiers a relation covers. Globs are shell-style: '*' does not cross '/'.
pub enum Pattern {
    All,
    Glob(GlobMatcher),
}

impl Pattern {
    pub fn glob(pat: &str) -> Result<Pattern> {
        let matcher = GlobBuilder::new(pat).literal_separator(true).build()?.compile_matcher();
        Ok(Pattern::Glob(matcher))
    }

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
