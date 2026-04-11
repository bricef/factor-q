//! In-memory registry of loaded agent definitions.
//!
//! Scans a directory for `.md` files, parses each one, and exposes the
//! resulting agents keyed by id. Per-file parse errors are collected
//! separately so a single broken definition does not prevent the rest of
//! the registry from loading.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::{definition::parse_agent, definition::ParseError, Agent, AgentId};

/// Result of scanning a directory for agent definitions.
#[derive(Debug, Default)]
pub struct AgentRegistry {
    agents: HashMap<AgentId, LoadedAgent>,
    errors: Vec<LoadError>,
}

/// A loaded agent with its source file path, for diagnostics and
/// hot-reload support.
#[derive(Debug, Clone)]
pub struct LoadedAgent {
    pub path: PathBuf,
    pub agent: Agent,
}

impl AgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every `.md` file under the given directory into the registry.
    ///
    /// Walks the directory recursively. Files whose contents fail to parse
    /// are recorded as errors in the registry but do not cause this
    /// function to return an error. An `Err` is returned only for I/O
    /// problems accessing the directory itself.
    pub fn load_from_directory(dir: &Path) -> Result<Self, RegistryError> {
        if !dir.exists() {
            return Err(RegistryError::DirectoryNotFound(dir.to_path_buf()));
        }
        if !dir.is_dir() {
            return Err(RegistryError::NotADirectory(dir.to_path_buf()));
        }

        let mut registry = Self::new();

        for entry in WalkDir::new(dir).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    registry.errors.push(LoadError::Walk {
                        message: err.to_string(),
                    });
                    continue;
                }
            };

            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }

            registry.load_file(entry.path());
        }

        Ok(registry)
    }

    /// Load a single agent definition file into the registry.
    pub fn load_file(&mut self, path: &Path) {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(err) => {
                self.errors.push(LoadError::ReadFile {
                    path: path.to_path_buf(),
                    source: err,
                });
                return;
            }
        };

        let agent = match parse_agent(&content) {
            Ok(agent) => agent,
            Err(err) => {
                self.errors.push(LoadError::Parse {
                    path: path.to_path_buf(),
                    source: err,
                });
                return;
            }
        };

        let id = agent.id().clone();
        if let Some(existing) = self.agents.get(&id) {
            self.errors.push(LoadError::DuplicateId {
                id: id.as_str().to_string(),
                first: existing.path.clone(),
                second: path.to_path_buf(),
            });
            return;
        }

        self.agents.insert(
            id,
            LoadedAgent {
                path: path.to_path_buf(),
                agent,
            },
        );
    }

    /// Look up an agent by id.
    pub fn get(&self, id: &AgentId) -> Option<&Agent> {
        self.agents.get(id).map(|l| &l.agent)
    }

    /// Look up a loaded agent (including its source path) by id.
    pub fn get_loaded(&self, id: &AgentId) -> Option<&LoadedAgent> {
        self.agents.get(id)
    }

    /// Iterate over all successfully loaded agents.
    pub fn iter(&self) -> impl Iterator<Item = &LoadedAgent> {
        self.agents.values()
    }

    /// Number of successfully loaded agents.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// True if no agents were loaded successfully.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Errors encountered during loading. Each error is scoped to a single
    /// file and does not affect other files.
    pub fn errors(&self) -> &[LoadError] {
        &self.errors
    }
}

/// An error loading a specific agent definition file.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: ParseError,
    },

    #[error("duplicate agent id '{id}' defined in {first} and {second}")]
    DuplicateId {
        id: String,
        first: PathBuf,
        second: PathBuf,
    },

    #[error("directory walk error: {message}")]
    Walk { message: String },
}

/// Errors that prevent the registry from being built at all (distinct from
/// per-file errors collected inside the registry).
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("directory does not exist: {0}")]
    DirectoryNotFound(PathBuf),

    #[error("path is not a directory: {0}")]
    NotADirectory(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    fn minimal_agent(name: &str) -> String {
        format!(
            r#"---
name: {name}
model: claude-haiku
---

You are an agent.
"#
        )
    }

    #[test]
    fn loads_single_agent_from_flat_directory() {
        let dir = tempdir().unwrap();
        write(dir.path(), "researcher.md", &minimal_agent("researcher"));

        let registry = AgentRegistry::load_from_directory(dir.path()).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.errors().is_empty());

        let id = AgentId::new("researcher").unwrap();
        assert!(registry.get(&id).is_some());
    }

    #[test]
    fn loads_recursively_from_subdirectories() {
        let dir = tempdir().unwrap();
        write(dir.path(), "research/scout.md", &minimal_agent("scout"));
        write(dir.path(), "ops/responder.md", &minimal_agent("responder"));
        write(dir.path(), "planner.md", &minimal_agent("planner"));

        let registry = AgentRegistry::load_from_directory(dir.path()).unwrap();
        assert_eq!(registry.len(), 3);
        assert!(registry.errors().is_empty());

        for name in ["scout", "responder", "planner"] {
            let id = AgentId::new(name).unwrap();
            assert!(registry.get(&id).is_some(), "missing {name}");
        }
    }

    #[test]
    fn ignores_non_markdown_files() {
        let dir = tempdir().unwrap();
        write(dir.path(), "agent.md", &minimal_agent("good"));
        write(dir.path(), "notes.txt", "ignored");
        write(dir.path(), "config.yaml", "ignored: true");

        let registry = AgentRegistry::load_from_directory(dir.path()).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.errors().is_empty());
    }

    #[test]
    fn collects_parse_errors_without_failing_others() {
        let dir = tempdir().unwrap();
        write(dir.path(), "good.md", &minimal_agent("good"));
        write(dir.path(), "broken.md", "not a valid definition");
        write(dir.path(), "also_good.md", &minimal_agent("other"));

        let registry = AgentRegistry::load_from_directory(dir.path()).unwrap();
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.errors().len(), 1);
        assert!(matches!(registry.errors()[0], LoadError::Parse { .. }));
    }

    #[test]
    fn detects_duplicate_ids() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a/agent.md", &minimal_agent("same"));
        write(dir.path(), "b/agent.md", &minimal_agent("same"));

        let registry = AgentRegistry::load_from_directory(dir.path()).unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.errors().len(), 1);
        assert!(matches!(
            &registry.errors()[0],
            LoadError::DuplicateId { id, .. } if id == "same"
        ));
    }

    #[test]
    fn missing_directory_is_error() {
        let err = AgentRegistry::load_from_directory(Path::new(
            "/tmp/factor-q-test-does-not-exist-xyz",
        ))
        .unwrap_err();
        assert!(matches!(err, RegistryError::DirectoryNotFound(_)));
    }

    #[test]
    fn empty_directory_returns_empty_registry() {
        let dir = tempdir().unwrap();
        let registry = AgentRegistry::load_from_directory(dir.path()).unwrap();
        assert!(registry.is_empty());
        assert!(registry.errors().is_empty());
    }
}
