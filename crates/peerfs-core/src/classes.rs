//! Path classes: which paths of a workspace are committed, and which stay on this host.
//!
//! - **Source** paths are committed, snapshotted and replicated.
//! - **Derived** paths (dependencies and build output) stay as files in the workspace's
//!   overlay across commits. Nothing commits or replicates them.
//! - **Secret** paths (credentials and keys) stay in the overlay too, and are never
//!   hashed into an object.
//!
//! Rules are gitignore patterns. Built-in defaults always apply. A `.gitignore` in the
//! workspace can only add derived paths. The owner's `classes` file in the space adds
//! rules per class, and can name paths as source. An owner's source rule wins over
//! everything; otherwise secret wins over derived. Paths are classified top-down, and
//! everything under a derived or secret directory takes its class, so `.gitignore`
//! files inside dependency trees are never read.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::error::{Error, Result};
use crate::path::TreePath;
use crate::workspace::EntryKind;

/// What happens to a path when its workspace commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Committed and replicated.
    Source,
    /// Dependencies and build output: kept in the workspace, never committed.
    Derived,
    /// Credentials and keys: kept in the workspace, never committed or hashed.
    Secret,
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Source => "source",
            Self::Derived => "derived",
            Self::Secret => "secret",
        })
    }
}

/// Dependency and build directories, tool caches, and transient files.
pub const DEFAULT_DERIVED: &[&str] = &[
    ".git/",
    "node_modules/",
    "target/",
    ".venv/",
    "venv/",
    "dist/",
    "build/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".tox/",
    ".next/",
    ".turbo/",
    ".gradle/",
    "*.tsbuildinfo",
    "*.egg-info/",
    "*.swp",
    "*~",
    "*-wal",
    "*-shm",
    "*-journal",
];

/// Credential and key files, after gitrob's filename signatures, with the templates and
/// public keys that are committed on purpose left out.
pub const DEFAULT_SECRET: &[&str] = &[
    ".env",
    ".env.*",
    "*.env",
    "!.env.example",
    "!.env.sample",
    "!.env.template",
    "!.env.dist",
    "!.env.defaults",
    ".envrc",
    ".netrc",
    "_netrc",
    ".npmrc",
    ".pypirc",
    ".yarnrc.yml",
    ".git-credentials",
    ".pgpass",
    ".my.cnf",
    ".s3cfg",
    ".htpasswd",
    ".dockercfg",
    ".docker/config.json",
    ".aws/credentials",
    ".gem/credentials",
    ".config/gcloud/",
    "kubeconfig",
    ".kube/config",
    "*.tfstate",
    "*.tfstate.backup",
    "*.tfvars",
    "!*.tfvars.example",
    ".terraform/",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "*_rsa",
    "*_ed25519",
    "!*.pub",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.pkcs12",
    "*.jks",
    "*.keystore",
    "*.kdbx",
    "*.ovpn",
    "credentials.json",
    "client_secret*.json",
    "service-account*.json",
    "*-sa-key.json",
    "secrets.yml",
    "secrets.yaml",
    "secrets.json",
    ".secrets/",
    ".claude/settings.local.json",
    ".mcp.json",
];

/// The owner's rules, from a space's `classes` file: gitignore lines under `[source]`,
/// `[derived]` and `[secret]` headings; `#` starts a comment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerRules {
    /// Paths that are source whatever else matches them.
    pub source: Vec<String>,
    /// Paths added to the derived class.
    pub derived: Vec<String>,
    /// Paths added to the secret class.
    pub secret: Vec<String>,
}

impl OwnerRules {
    /// Reads the rules at `path`; a missing file has none.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read, or has a rule outside a known heading.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(Error::io("reading", path, e)),
        };
        Self::parse(&text).map_err(|line| {
            Error::Corrupt(format!(
                "{}: {line:?} is not under [source], [derived] or [secret]",
                path.display()
            ))
        })
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut rules = Self::default();
        let mut section: Option<&mut Vec<String>> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            section = match line {
                "[source]" => Some(&mut rules.source),
                "[derived]" => Some(&mut rules.derived),
                "[secret]" => Some(&mut rules.secret),
                rule => {
                    let Some(lines) = section else {
                        return Err(rule.to_owned());
                    };
                    lines.push(rule.to_owned());
                    Some(lines)
                }
            };
        }
        Ok(rules)
    }
}

/// Reads the `.gitignore` in a directory of a workspace, if there is one.
type ReadGitignore<'read> = Box<dyn Fn(&TreePath) -> Option<Vec<u8>> + 'read>;

/// Classifies the paths of one view of a workspace.
pub(crate) struct Classifier<'read> {
    source: Gitignore,
    secret: Gitignore,
    derived: Gitignore,
    read_gitignore: ReadGitignore<'read>,
    gitignores: HashMap<TreePath, Option<Gitignore>>,
}

impl fmt::Debug for Classifier<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Classifier").finish_non_exhaustive()
    }
}

/// The virtual directory a tree path is matched under.
fn virtual_dir(dir: &TreePath) -> PathBuf {
    let mut path = PathBuf::from("/");
    for name in dir.components() {
        path.push(name.as_str());
    }
    path
}

fn matcher<'line>(root: &Path, lines: impl IntoIterator<Item = &'line str>) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for line in lines {
        // A malformed rule is skipped, as git skips it.
        let _ = builder.add_line(None, line);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

impl<'read> Classifier<'read> {
    /// A classifier with the default rules, the owner's, and the workspace's `.gitignore`
    /// files as `read_gitignore` reads them.
    pub(crate) fn new(
        owner: &OwnerRules,
        read_gitignore: impl Fn(&TreePath) -> Option<Vec<u8>> + 'read,
    ) -> Self {
        let root = Path::new("/");
        Self {
            source: matcher(root, owner.source.iter().map(String::as_str)),
            secret: matcher(
                root,
                DEFAULT_SECRET
                    .iter()
                    .copied()
                    .chain(owner.secret.iter().map(String::as_str)),
            ),
            derived: matcher(
                root,
                DEFAULT_DERIVED
                    .iter()
                    .copied()
                    .chain(owner.derived.iter().map(String::as_str)),
            ),
            read_gitignore: Box::new(read_gitignore),
            gitignores: HashMap::new(),
        }
    }

    /// The class of `path` given that its parent directory is source.
    pub(crate) fn classify_child(&mut self, path: &TreePath, kind: EntryKind) -> Class {
        let is_dir = kind == EntryKind::Directory;
        let full = virtual_dir(path);
        if self.source.matched(&full, is_dir).is_ignore() {
            return Class::Source;
        }
        if self.secret.matched(&full, is_dir).is_ignore() {
            return Class::Secret;
        }
        if self.derived.matched(&full, is_dir).is_ignore() || self.gitignored(path, kind) {
            return Class::Derived;
        }
        Class::Source
    }

    /// The class of `path`, checking its ancestors first.
    #[cfg(test)]
    pub(crate) fn classify(&mut self, path: &TreePath, kind: EntryKind) -> Class {
        let names = path.components();
        let mut ancestor = TreePath::root();
        for name in names.iter().take(names.len().saturating_sub(1)) {
            ancestor = ancestor.join(name);
            let class = self.classify_child(&ancestor, EntryKind::Directory);
            if class != Class::Source {
                return class;
            }
        }
        self.classify_child(path, kind)
    }

    /// Whether the `.gitignore` files above `path` ignore it, the deepest deciding.
    fn gitignored(&mut self, path: &TreePath, kind: EntryKind) -> bool {
        let is_dir = kind == EntryKind::Directory;
        let full = virtual_dir(path);
        let mut dirs = Vec::new();
        let mut dir = path.split_last().map(|(parent, _)| parent);
        while let Some(current) = dir {
            dir = current.split_last().map(|(parent, _)| parent);
            dirs.push(current);
        }
        for dir in dirs {
            let matched = match self.gitignore(&dir) {
                Some(gitignore) => gitignore.matched(&full, is_dir),
                None => Match::None,
            };
            match matched {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }

    fn gitignore(&mut self, dir: &TreePath) -> Option<&Gitignore> {
        if !self.gitignores.contains_key(dir) {
            let parsed = (self.read_gitignore)(dir).map(|bytes| {
                let text = String::from_utf8_lossy(&bytes);
                matcher(&virtual_dir(dir), text.lines())
            });
            self.gitignores.insert(dir.clone(), parsed);
        }
        self.gitignores.get(dir).and_then(Option::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> TreePath {
        TreePath::try_from(text).expect("valid path")
    }

    fn classify(
        owner: &OwnerRules,
        gitignores: &[(&str, &str)],
        text: &str,
        kind: EntryKind,
    ) -> Class {
        let files: HashMap<TreePath, Vec<u8>> = gitignores
            .iter()
            .map(|(dir, body)| (path(dir), body.as_bytes().to_vec()))
            .collect();
        let mut classifier = Classifier::new(owner, |dir| files.get(dir).cloned());
        classifier.classify(&path(text), kind)
    }

    #[test]
    fn defaults_class_dependencies_build_output_and_credentials() {
        let none = OwnerRules::default();
        for (text, kind, class) in [
            ("src/main.rs", EntryKind::File, Class::Source),
            ("target", EntryKind::Directory, Class::Derived),
            ("crates/a/target/debug/a", EntryKind::File, Class::Derived),
            (
                "web/node_modules/x/index.js",
                EntryKind::File,
                Class::Derived,
            ),
            (".git/HEAD", EntryKind::File, Class::Derived),
            ("pkg/__pycache__/m.pyc", EntryKind::File, Class::Derived),
            (".env", EntryKind::File, Class::Secret),
            ("api/.env.local", EntryKind::File, Class::Secret),
            (".env.example", EntryKind::File, Class::Source),
            ("deploy/id_ed25519", EntryKind::File, Class::Secret),
            ("deploy/id_ed25519.pub", EntryKind::File, Class::Source),
            ("infra/prod.tfvars", EntryKind::File, Class::Secret),
            ("build.rs", EntryKind::File, Class::Source),
        ] {
            assert_eq!(classify(&none, &[], text, kind), class, "{text}");
        }
    }

    #[test]
    fn gitignore_only_adds_derived_paths_and_the_deepest_file_decides() {
        let none = OwnerRules::default();
        let gitignores = [
            (".", "*.log\ngenerated/\n!keep.log\n"),
            ("web", "out/\n!important.log\n"),
        ];
        assert_eq!(
            classify(&none, &gitignores, "a.log", EntryKind::File),
            Class::Derived
        );
        assert_eq!(
            classify(&none, &gitignores, "keep.log", EntryKind::File),
            Class::Source
        );
        assert_eq!(
            classify(&none, &gitignores, "web/important.log", EntryKind::File),
            Class::Source
        );
        assert_eq!(
            classify(&none, &gitignores, "web/out/x.js", EntryKind::File),
            Class::Derived
        );
        assert_eq!(
            classify(&none, &gitignores, "generated/x.rs", EntryKind::File),
            Class::Derived
        );
        // A .gitignore cannot make a default derived or secret path source.
        let whitelisting = [(".", "!target/\n!.env\n")];
        assert_eq!(
            classify(&none, &whitelisting, "target/x", EntryKind::File),
            Class::Derived
        );
        assert_eq!(
            classify(&none, &whitelisting, ".env", EntryKind::File),
            Class::Secret
        );
    }

    #[test]
    fn owner_rules_add_classes_and_name_source_over_everything() {
        let owner = OwnerRules::parse(
            "# owner rules\n[source]\nbuild/\n.npmrc\n[derived]\ncache/\n[secret]\n*.token\n",
        )
        .expect("rules");
        assert_eq!(
            classify(&owner, &[], "build/script.sh", EntryKind::File),
            Class::Source
        );
        assert_eq!(
            classify(&owner, &[], ".npmrc", EntryKind::File),
            Class::Source
        );
        assert_eq!(
            classify(&owner, &[], "cache/a", EntryKind::File),
            Class::Derived
        );
        assert_eq!(
            classify(&owner, &[], "ci/deploy.token", EntryKind::File),
            Class::Secret
        );
        assert_eq!(OwnerRules::parse("stray\n"), Err("stray".to_owned()));
    }

    #[test]
    fn gitignores_inside_derived_directories_are_never_read() {
        let none = OwnerRules::default();
        let files: HashMap<TreePath, Vec<u8>> = HashMap::new();
        let mut read = Vec::new();
        let cell = std::cell::RefCell::new(&mut read);
        let mut classifier = Classifier::new(&none, |dir: &TreePath| {
            cell.borrow_mut().push(dir.to_string());
            files.get(dir).cloned()
        });
        assert_eq!(
            classifier.classify(&path("node_modules/a/b/c.js"), EntryKind::File),
            Class::Derived
        );
        drop(classifier);
        assert!(
            read.iter().all(|dir| !dir.contains("node_modules")),
            "{read:?}"
        );
    }
}
