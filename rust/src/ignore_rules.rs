use std::fs;
use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

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

#[derive(Debug)]
struct RuleLayer {
    matcher: Gitignore,
}

impl RuleLayer {
    fn from_lines(
        root: &Path,
        lines: impl IntoIterator<Item = String>,
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
    pub fn new(
        root: &Path,
        runtime_patterns: impl IntoIterator<Item = String>,
    ) -> Result<Self, IgnoreError> {
        Ok(Self {
            hard: RuleLayer::from_lines(root, HARD_PATTERNS.iter().map(|v| (*v).to_owned()))?,
            runtime: RuleLayer::from_lines(root, runtime_patterns)?,
            oce: RuleLayer::from_lines(root, read_lines(root.join(".oceignore")))?,
            git: RuleLayer::from_lines(root, read_lines(root.join(".gitignore")))?,
            defaults: RuleLayer::from_lines(
                root,
                DEFAULT_PATTERNS.iter().map(|v| (*v).to_owned()),
            )?,
        })
    }

    pub fn ignores(&self, path: &str, is_dir: bool) -> bool {
        let mut normalized = path.replace('\\', "/");
        while let Some(without_prefix) = normalized.strip_prefix("./") {
            normalized = without_prefix.to_owned();
        }
        if self
            .hard
            .decision(&normalized.to_lowercase(), is_dir)
            .is_some_and(|decision| decision)
        {
            return true;
        }
        for layer in [&self.runtime, &self.oce, &self.git, &self.defaults] {
            if let Some(decision) = layer.decision(&normalized, is_dir) {
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
