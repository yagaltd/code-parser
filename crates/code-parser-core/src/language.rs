/// Language identifiers matching the tree-sitter grammar names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
}

impl Language {
    /// Detect language from file extension.
    pub fn from_path(path: &str) -> Option<Self> {
        if path.ends_with(".rs") {
            Some(Self::Rust)
        } else if path.ends_with(".tsx") {
            Some(Self::TypeScript)
        } else if path.ends_with(".ts") {
            Some(Self::TypeScript)
        } else if path.ends_with(".jsx") {
            Some(Self::JavaScript)
        } else if path.ends_with(".mjs") || path.ends_with(".cjs") || path.ends_with(".js") {
            Some(Self::JavaScript)
        } else if path.ends_with(".py") || path.ends_with(".pyi") {
            Some(Self::Python)
        } else {
            None
        }
    }

    /// Human-readable name matching IR `language` field.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::TypeScript => "TypeScript",
            Self::JavaScript => "JavaScript",
            Self::Python => "Python",
        }
    }
}
