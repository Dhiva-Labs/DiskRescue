//! Turn the flat path lists of an [`FsSpec`] into a directory tree.
//!
//! The plan fixes the order in which things are allocated: explicit `dirs`
//! first, then the files in spec order, each preceded by any parent directory
//! it needs that does not exist yet. Deletions are resolved here too
//! (deleting a directory deletes everything below it).

use std::collections::HashMap;

use crate::error::{FixtureError, Result};
use crate::fs::FsSpec;

/// Timestamp used when a file has no explicit mtime: 2024-05-06 07:08:09 UTC.
pub const DEFAULT_MTIME: i64 = 1_714_979_289;

/// A directory in the plan. Index 0 is the root.
#[derive(Debug, Clone)]
pub(crate) struct DirNode {
    /// Component name (empty for the root).
    pub name: String,
    /// Normalised path (`""` for the root, `a/b` otherwise).
    pub path: String,
    /// Parent directory index.
    pub parent: Option<usize>,
    /// Children in creation order.
    pub children: Vec<Child>,
    /// Deleted after population.
    pub deleted: bool,
    /// Timestamp.
    pub mtime: i64,
}

/// A file in the plan.
#[derive(Debug, Clone)]
pub(crate) struct FileNode {
    /// Component name.
    pub name: String,
    /// Normalised path.
    pub path: String,
    /// Index into `FsSpec::files`.
    pub spec: usize,
    /// Deleted after population.
    pub deleted: bool,
    /// Timestamp.
    pub mtime: i64,
}

/// A directory entry reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Child {
    /// Subdirectory index.
    Dir(usize),
    /// File index.
    File(usize),
}

/// The whole tree plus the allocation order.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub dirs: Vec<DirNode>,
    pub files: Vec<FileNode>,
    /// Allocation order: every dir and file exactly once, parents first.
    pub order: Vec<Child>,
}

/// Split a path into validated components.
pub(crate) fn components(path: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for c in path.split('/') {
        if c.is_empty() {
            continue;
        }
        if c == "." || c == ".." || c.contains('\0') || c.contains('\\') {
            return Err(FixtureError::invalid(format!(
                "bad path component in {path:?}"
            )));
        }
        out.push(c.to_string());
    }
    if out.is_empty() {
        return Err(FixtureError::invalid(format!("empty path {path:?}")));
    }
    Ok(out)
}

fn key(name: &str, case_insensitive: bool) -> String {
    if case_insensitive {
        name.to_uppercase()
    } else {
        name.to_string()
    }
}

impl Plan {
    /// Build the plan. `case_insensitive` makes name collisions inside one
    /// directory an error the way the real filesystem would see them.
    pub fn new(spec: &FsSpec, case_insensitive: bool) -> Result<Plan> {
        let mut p = Plan {
            dirs: vec![DirNode {
                name: String::new(),
                path: String::new(),
                parent: None,
                children: Vec::new(),
                deleted: false,
                mtime: super::DEFAULT_MTIME,
            }],
            files: Vec::new(),
            order: Vec::new(),
        };
        let mut index: HashMap<(usize, String), Child> = HashMap::new();
        for d in &spec.dirs {
            let comps = components(d)?;
            p.ensure_dir(&comps, &mut index, case_insensitive)?;
        }
        for (i, f) in spec.files.iter().enumerate() {
            let mut comps = components(&f.path)?;
            let name = comps.pop().unwrap_or_default();
            let dir = p.ensure_dir(&comps, &mut index, case_insensitive)?;
            let k = (dir, key(&name, case_insensitive));
            if index.contains_key(&k) {
                return Err(FixtureError::invalid(format!(
                    "duplicate path {:?}",
                    f.path
                )));
            }
            let idx = p.files.len();
            let mut path = p.dirs[dir].path.clone();
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(&name);
            p.files.push(FileNode {
                name,
                path,
                spec: i,
                deleted: false,
                mtime: f.mtime.unwrap_or(super::DEFAULT_MTIME),
            });
            index.insert(k, Child::File(idx));
            p.dirs[dir].children.push(Child::File(idx));
            p.order.push(Child::File(idx));
        }
        for d in &spec.deleted {
            let comps = components(d)?;
            let mut cur = 0usize;
            let mut found: Option<Child> = Some(Child::Dir(0));
            for c in &comps {
                match index.get(&(cur, key(c, case_insensitive))) {
                    Some(Child::Dir(n)) => {
                        cur = *n;
                        found = Some(Child::Dir(*n));
                    }
                    Some(Child::File(n)) => {
                        found = Some(Child::File(*n));
                        break;
                    }
                    None => {
                        found = None;
                        break;
                    }
                }
            }
            match found {
                Some(Child::File(n)) => p.files[n].deleted = true,
                Some(Child::Dir(n)) if n != 0 => p.mark_dir_deleted(n),
                _ => {
                    return Err(FixtureError::invalid(format!(
                        "cannot delete {d:?}: no such file or directory in spec"
                    )))
                }
            }
        }
        Ok(p)
    }

    fn mark_dir_deleted(&mut self, d: usize) {
        self.dirs[d].deleted = true;
        let kids = self.dirs[d].children.clone();
        for c in kids {
            match c {
                Child::Dir(n) => self.mark_dir_deleted(n),
                Child::File(n) => self.files[n].deleted = true,
            }
        }
    }

    fn ensure_dir(
        &mut self,
        comps: &[String],
        index: &mut HashMap<(usize, String), Child>,
        ci: bool,
    ) -> Result<usize> {
        let mut cur = 0usize;
        for c in comps {
            let k = (cur, key(c, ci));
            match index.get(&k) {
                Some(Child::Dir(n)) => cur = *n,
                Some(Child::File(_)) => {
                    return Err(FixtureError::invalid(format!(
                        "{c:?} is both a file and a directory"
                    )))
                }
                None => {
                    let idx = self.dirs.len();
                    let mut path = self.dirs[cur].path.clone();
                    if !path.is_empty() {
                        path.push('/');
                    }
                    path.push_str(c);
                    self.dirs.push(DirNode {
                        name: c.clone(),
                        path,
                        parent: Some(cur),
                        children: Vec::new(),
                        deleted: false,
                        mtime: super::DEFAULT_MTIME,
                    });
                    index.insert(k, Child::Dir(idx));
                    self.dirs[cur].children.push(Child::Dir(idx));
                    self.order.push(Child::Dir(idx));
                    cur = idx;
                }
            }
        }
        Ok(cur)
    }

    /// Paths of directories from `d` up to (excluding) the root.
    pub fn depth(&self, mut d: usize) -> usize {
        let mut n = 0;
        while let Some(p) = self.dirs[d].parent {
            n += 1;
            d = p;
        }
        n
    }
}

/// Decompose a Unix timestamp to `(year, month, day, hour, minute, second)`.
pub(crate) fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    diskrescue_core::time::Timestamp::from_unix(secs).civil()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSpec, FsType};

    #[test]
    fn builds_tree_in_order() {
        let mut spec = FsSpec::new(FsType::Fat32, 1 << 20);
        spec.dirs.push("empty".into());
        spec.files.push(FileSpec::new("a/b/one.txt", vec![1]));
        spec.files.push(FileSpec::new("/a/two.txt", vec![2]));
        spec.files.push(FileSpec::new("top.txt", vec![3]));
        spec.deleted.push("a/b".into());
        let p = Plan::new(&spec, true).unwrap();
        assert_eq!(p.dirs.len(), 4);
        assert_eq!(p.dirs[1].path, "empty");
        assert_eq!(p.dirs[2].path, "a");
        assert_eq!(p.dirs[3].path, "a/b");
        assert!(p.dirs[3].deleted && !p.dirs[2].deleted);
        assert!(p.files[0].deleted && !p.files[1].deleted);
        assert_eq!(p.files[0].path, "a/b/one.txt");
        assert_eq!(p.order.len(), 6);
        assert_eq!(p.depth(3), 2);
    }

    #[test]
    fn rejects_bad_specs() {
        let mk = |files: Vec<&str>, del: Vec<&str>| {
            let mut s = FsSpec::new(FsType::Fat32, 1 << 20);
            for f in files {
                s.files.push(FileSpec::new(f, vec![]));
            }
            s.deleted = del.into_iter().map(String::from).collect();
            Plan::new(&s, true)
        };
        assert!(mk(vec!["a/../b"], vec![]).is_err());
        assert!(mk(vec![""], vec![]).is_err());
        assert!(mk(vec!["A.txt", "a.TXT"], vec![]).is_err());
        assert!(mk(vec!["a", "a/b"], vec![]).is_err());
        assert!(mk(vec!["a"], vec!["nope"]).is_err());
        assert!(mk(vec!["a"], vec!["/"]).is_err());
        assert!(mk(vec!["x\\y"], vec![]).is_err());
    }
}
