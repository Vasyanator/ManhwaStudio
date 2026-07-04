/*
File: mem.rs

Purpose:
In-memory virtual-filesystem backend for the `Storage` trait. Serves as the web
build's session store: the application hydrates it from browser IndexedDB at
startup and flushes it back at save checkpoints (that async bridge lives in the
app's web layer, not here). Also the default backend for unit tests.

Key structures:
- MemStorage: cloneable, thread-safe handle over a shared directory tree.
- Node:       internal file/dir tree node.

Notes:
Thread-safe via `RwLock`; clones share the same tree through `Arc`, matching how
the application shares one `Arc<dyn Storage>` across worker threads. Semantics
mirror `NativeStorage` exactly so code behaves identically on both backends.
*/

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::{DirEntry, Metadata, Storage, StorageError, path};

/// A node in the in-memory tree: either a directory (named children) or a file.
#[derive(Debug)]
enum Node {
    Dir(BTreeMap<String, Node>),
    File(Vec<u8>),
}

impl Node {
    fn as_dir(&self) -> Option<&BTreeMap<String, Node>> {
        match self {
            Node::Dir(m) => Some(m),
            Node::File(_) => None,
        }
    }

    fn as_dir_mut(&mut self) -> Option<&mut BTreeMap<String, Node>> {
        match self {
            Node::Dir(m) => Some(m),
            Node::File(_) => None,
        }
    }
}

/// In-memory [`Storage`] backend.
///
/// Cloning yields another handle to the *same* tree (shared `Arc`), so the app
/// can pass clones to worker threads exactly like the native backend.
#[derive(Debug, Clone)]
pub struct MemStorage {
    root: Arc<RwLock<Node>>,
}

impl Default for MemStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl MemStorage {
    /// Creates an empty in-memory store (just the root directory).
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: Arc::new(RwLock::new(Node::Dir(BTreeMap::new()))),
        }
    }

    /// Poison recovery: a panicked lock still holds a valid tree, so we take the
    /// inner value rather than propagate the `PoisonError` as a storage error.
    fn read_root(&self) -> std::sync::RwLockReadGuard<'_, Node> {
        self.root.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_root(&self) -> std::sync::RwLockWriteGuard<'_, Node> {
        self.root.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Walks `comps` from `node`, returning the addressed node if present.
fn lookup<'a>(node: &'a Node, comps: &[String]) -> Option<&'a Node> {
    let mut cur = node;
    for c in comps {
        cur = cur.as_dir()?.get(c)?;
    }
    Some(cur)
}

impl Storage for MemStorage {
    fn read(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        let comps = path::normalize(path)?;
        let root = self.read_root();
        match lookup(&root, &comps) {
            Some(Node::File(data)) => Ok(data.clone()),
            Some(Node::Dir(_)) => Err(StorageError::IsADirectory(path.to_string())),
            None => Err(StorageError::NotFound(path.to_string())),
        }
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        let comps = path::normalize(path)?;
        let (name, parents) = comps
            .split_last()
            .ok_or_else(|| StorageError::IsADirectory(path.to_string()))?;
        let mut root = self.write_root();
        // Navigate to the parent directory; it must already exist (std::fs-like).
        let mut cur = &mut *root;
        for p in parents {
            cur = cur
                .as_dir_mut()
                .ok_or_else(|| StorageError::NotADirectory(path.to_string()))?
                .get_mut(p)
                .ok_or_else(|| StorageError::NotFound(path.to_string()))?;
        }
        let dir = cur
            .as_dir_mut()
            .ok_or_else(|| StorageError::NotADirectory(path.to_string()))?;
        if let Some(Node::Dir(_)) = dir.get_mut(name) {
            // Refuse to clobber an existing directory with a file.
            Err(StorageError::IsADirectory(path.to_string()))
        } else {
            dir.insert(name.clone(), Node::File(data.to_vec()));
            Ok(())
        }
    }

    fn exists(&self, path: &str) -> bool {
        let Ok(comps) = path::normalize(path) else {
            return false;
        };
        let root = self.read_root();
        lookup(&root, &comps).is_some()
    }

    fn is_dir(&self, path: &str) -> bool {
        let Ok(comps) = path::normalize(path) else {
            return false;
        };
        let root = self.read_root();
        matches!(lookup(&root, &comps), Some(Node::Dir(_)))
    }

    fn create_dir_all(&self, path: &str) -> Result<(), StorageError> {
        let comps = path::normalize(path)?;
        let mut root = self.write_root();
        let mut cur = &mut *root;
        for c in &comps {
            let dir = cur
                .as_dir_mut()
                .ok_or_else(|| StorageError::NotADirectory(path.to_string()))?;
            let next = dir
                .entry(c.clone())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
            if next.as_dir().is_none() {
                // A file already occupies this component.
                return Err(StorageError::NotADirectory(path.to_string()));
            }
            cur = next;
        }
        Ok(())
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let comps = path::normalize(path)?;
        let root = self.read_root();
        match lookup(&root, &comps) {
            Some(Node::Dir(m)) => Ok(m
                .iter()
                .map(|(name, node)| DirEntry {
                    name: name.clone(),
                    is_dir: node.as_dir().is_some(),
                })
                .collect()),
            Some(Node::File(_)) => Err(StorageError::NotADirectory(path.to_string())),
            None => Err(StorageError::NotFound(path.to_string())),
        }
    }

    fn remove_file(&self, path: &str) -> Result<(), StorageError> {
        let comps = path::normalize(path)?;
        let (name, parents) = comps
            .split_last()
            .ok_or_else(|| StorageError::IsADirectory(path.to_string()))?;
        let mut root = self.write_root();
        let dir = dir_at_mut(&mut root, parents, path)?;
        match dir.get(name) {
            Some(Node::File(_)) => {
                dir.remove(name);
                Ok(())
            }
            Some(Node::Dir(_)) => Err(StorageError::IsADirectory(path.to_string())),
            None => Err(StorageError::NotFound(path.to_string())),
        }
    }

    fn remove_dir_all(&self, path: &str) -> Result<(), StorageError> {
        let comps = path::normalize(path)?;
        let (name, parents) = comps
            .split_last()
            .ok_or_else(|| StorageError::Backend("cannot remove storage root".into()))?;
        let mut root = self.write_root();
        let dir = dir_at_mut(&mut root, parents, path)?;
        match dir.get(name) {
            Some(Node::Dir(_)) => {
                dir.remove(name);
                Ok(())
            }
            Some(Node::File(_)) => Err(StorageError::NotADirectory(path.to_string())),
            None => Err(StorageError::NotFound(path.to_string())),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from_comps = path::normalize(from)?;
        let to_comps = path::normalize(to)?;
        let (from_name, from_parents) = from_comps
            .split_last()
            .ok_or_else(|| StorageError::Backend("cannot rename storage root".into()))?;
        let (to_name, to_parents) = to_comps
            .split_last()
            .ok_or_else(|| StorageError::Backend("cannot rename onto storage root".into()))?;
        let mut root = self.write_root();
        // Detach the source node first, then re-attach under the destination.
        let node = {
            let dir = dir_at_mut(&mut root, from_parents, from)?;
            dir.remove(from_name)
                .ok_or_else(|| StorageError::NotFound(from.to_string()))?
        };
        let dest = dir_at_mut(&mut root, to_parents, to)?;
        dest.insert(to_name.clone(), node);
        Ok(())
    }

    fn metadata(&self, path: &str) -> Result<Metadata, StorageError> {
        let comps = path::normalize(path)?;
        let root = self.read_root();
        match lookup(&root, &comps) {
            Some(Node::File(data)) => Ok(Metadata {
                // File sizes cannot realistically exceed u64; saturate rather
                // than panic to keep the method total.
                len: u64::try_from(data.len()).unwrap_or(u64::MAX),
                is_dir: false,
                // In-memory store keeps no modification timestamps.
                modified: None,
            }),
            Some(Node::Dir(_)) => Ok(Metadata {
                len: 0,
                is_dir: true,
                modified: None,
            }),
            None => Err(StorageError::NotFound(path.to_string())),
        }
    }
}

/// Navigates to the directory addressed by `parents`, returning its child map.
///
/// Errors with [`StorageError::NotFound`] if a parent is missing or
/// [`StorageError::NotADirectory`] if a parent is a file. `vpath` is only used
/// for error messages.
fn dir_at_mut<'a>(
    root: &'a mut Node,
    parents: &[String],
    vpath: &str,
) -> Result<&'a mut BTreeMap<String, Node>, StorageError> {
    let mut cur = root;
    for p in parents {
        cur = cur
            .as_dir_mut()
            .ok_or_else(|| StorageError::NotADirectory(vpath.to_string()))?
            .get_mut(p)
            .ok_or_else(|| StorageError::NotFound(vpath.to_string()))?;
    }
    cur.as_dir_mut()
        .ok_or_else(|| StorageError::NotADirectory(vpath.to_string()))
}
