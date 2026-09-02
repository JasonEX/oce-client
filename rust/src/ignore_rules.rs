use std::fs;
use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

const OCEIGNORE_NAME: &str = ".oceignore";
const GITIGNORE_NAME: &str = ".gitignore";

const DEFAULT_PATTERNS: &[&str] = &[
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".venv/",
    "venv/",
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    "coverage/",
    ".idea/",
    ".vscode/",
];

const HARD_PATTERNS: &[&str] = &[
    ".git/",
    ".git/**",
    ".oce-client/",
    ".oce-client/**",
    ".env",
    ".env.*",
    "!.env.example",
    "!.env.*.example",
    "!.env.sample",
    "!.env.*.sample",
    "!.env.template",
    "!.env.*.template",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "id_rsa",
    "id_ed25519",
    ".git-credentials",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".aws/",
    ".aws/**",
    ".ssh/",
    ".ssh/**",
];

/// Whether a path names one of the ignore files whose change requires a full reconcile.
pub fn is_ignore_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(OCEIGNORE_NAME | GITIGNORE_NAME)
    )
}

#[derive(Debug)]
struct RuleLayer {
    matcher: Gitignore,
}

impl RuleLayer {
    fn from_lines<'a>(
        root: &Path,
        lines: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, IgnoreError> {
        let mut builder = GitignoreBuilder::new(root);
        for raw in lines {
            let line = raw.trim_end_matches(['\r', '\n']);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            builder
                .add_line(None, line)
                .map_err(|source| IgnoreError::Pattern {
                    pattern: line.to_owned(),
                    source,
                })?;
        }
        let matcher = builder.build().map_err(IgnoreError::Build)?;
        Ok(Self { matcher })
    }

    fn decision(&self, path: &str, is_dir: bool) -> Option<bool> {
        match self
            .matcher
            .matched_path_or_any_parents(Path::new(path), is_dir)
        {
            Match::Ignore(_) => Some(true),
            Match::Whitelist(_) => Some(false),
            Match::None => None,
        }
    }
}

#[derive(Debug)]
pub struct LayeredIgnoreMatcher {
    hard: RuleLayer,
    runtime: RuleLayer,
    oce: RuleLayer,
    git: RuleLayer,
    defaults: RuleLayer,
}

impl LayeredIgnoreMatcher {
    pub fn new(root: &Path, runtime_patterns: &[String]) -> Result<Self, IgnoreError> {
        let oce_lines = read_lines(root.join(OCEIGNORE_NAME));
        let git_lines = read_lines(root.join(GITIGNORE_NAME));
        Ok(Self {
            hard: RuleLayer::from_lines(root, HARD_PATTERNS.iter().copied())?,
            runtime: RuleLayer::from_lines(root, runtime_patterns.iter().map(String::as_str))?,
            oce: RuleLayer::from_lines(root, oce_lines.iter().map(String::as_str))?,
            git: RuleLayer::from_lines(root, git_lines.iter().map(String::as_str))?,
            defaults: RuleLayer::from_lines(root, DEFAULT_PATTERNS.iter().copied())?,
        })
    }

    /// Decides admission for a normalized, slash-separated workspace-relative path.
    pub fn ignores(&self, path: &str, is_dir: bool) -> bool {
        if self
            .hard
            .decision(&path.to_lowercase(), is_dir)
            .is_some_and(|decision| decision)
        {
            return true;
        }
        for layer in [&self.runtime, &self.oce, &self.git, &self.defaults] {
            if let Some(decision) = layer.decision(path, is_dir) {
                return decision;
            }
        }
        false
    }
}

fn read_lines(path: PathBuf) -> Vec<String> {
    fs::read_to_string(path)
        .map(|contents| contents.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

#[derive(Debug, thiserror::Error)]
pub enum IgnoreError {
    #[error("invalid ignore pattern {pattern:?}: {source}")]
    Pattern {
        pattern: String,
        source: ignore::Error,
    },
    #[error("failed to build ignore matcher: {0}")]
    Build(ignore::Error),
}
