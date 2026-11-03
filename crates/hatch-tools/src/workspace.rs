//! The workspace: the one directory tree file tools and commands may touch.
//!
//! Paths from the model are resolved against the canonical root, normalised
//! lexically (a `..` that climbs above the root is refused), and then checked
//! on disk: the deepest existing ancestor must canonicalise inside the root, so
//! a symlink pointing outside is refused too. The final component is opened
//! with `O_NOFOLLOW`, and writes go to a temp file in the same directory that
//! is renamed over the target. A process racing to swap a parent directory for
//! a symlink between check and use is a residual risk (documented in SECURITY).

use hatch_core::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: &Path) -> Result<Workspace> {
        let root = root.canonicalize().map_err(|e| Error::Invalid(format!("workspace {}: {e}", root.display())))?;
        if !root.is_dir() {
            return Err(Error::Invalid(format!("workspace {} is not a directory", root.display())));
        }
        if root == Path::new("/") {
            return Err(Error::Invalid("the filesystem root cannot be a workspace".into()));
        }
        Ok(Workspace { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lexically normalise `input` against the root; refuse escapes.
    fn normalise(&self, input: &str) -> Result<PathBuf> {
        let input = input.trim();
        if input.is_empty() {
            return Err(Error::Invalid("empty path".into()));
        }
        let p = Path::new(input);
        let joined = if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) };
        let mut out = PathBuf::new();
        for c in joined.components() {
            match c {
                Component::RootDir => out.push("/"),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !out.pop() {
                        return Err(self.escape(input));
                    }
                }
                Component::Normal(s) => out.push(s),
                Component::Prefix(_) => return Err(self.escape(input)),
            }
        }
        if !out.starts_with(&self.root) {
            return Err(self.escape(input));
        }
        Ok(out)
    }

    fn escape(&self, input: &str) -> Error {
        Error::Denied(format!("路径 {input:?} 在工作区 {} 之外", self.root.display()))
    }

    /// Resolve a path for reading or writing. The target need not exist.
    pub fn resolve(&self, input: &str) -> Result<PathBuf> {
        let path = self.normalise(input)?;
        // Deepest existing ancestor (including the path itself) must stay inside.
        let mut probe = path.clone();
        loop {
            if std::fs::symlink_metadata(&probe).is_ok() {
                let real = probe.canonicalize().map_err(|e| Error::Invalid(format!("{}: {e}", probe.display())))?;
                if !real.starts_with(&self.root) {
                    return Err(self.escape(input));
                }
                break;
            }
            if !probe.pop() {
                return Err(self.escape(input));
            }
        }
        Ok(path)
    }

    /// Workspace-relative display form.
    pub fn rel(&self, abs: &Path) -> String {
        abs.strip_prefix(&self.root).map(|p| p.display().to_string()).unwrap_or_else(|_| abs.display().to_string())
    }

    /// Open a regular file without following a final symlink.
    pub fn open_read(&self, input: &str) -> Result<(PathBuf, File)> {
        let path = self.resolve(input)?;
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| Error::NotFound(format!("{}: {e}", self.rel(&path))))?;
        if !f.metadata()?.is_file() {
            return Err(Error::Invalid(format!("{} is not a regular file", self.rel(&path))));
        }
        Ok((path, f))
    }

    /// Current content of a file, or `None` when it does not exist. `limit` bytes max.
    pub fn read_bytes(&self, input: &str, limit: u64) -> Result<Option<Vec<u8>>> {
        let path = self.resolve(input)?;
        match std::fs::symlink_metadata(&path) {
            Err(_) => return Ok(None),
            Ok(m) if m.file_type().is_symlink() => return Err(Error::Denied(format!("{} is a symlink", self.rel(&path)))),
            Ok(m) if m.len() > limit => {
                return Err(Error::Invalid(format!("{} is larger than {} bytes", self.rel(&path), limit)))
            }
            Ok(_) => {}
        }
        let (_, mut f) = self.open_read(input)?;
        let mut buf = Vec::new();
        Read::by_ref(&mut f).take(limit + 1).read_to_end(&mut buf)?;
        Ok(Some(buf))
    }

    /// Atomically replace (or create) a file: temp file + rename.
    pub fn write_atomic(&self, input: &str, content: &[u8]) -> Result<PathBuf> {
        let path = self.resolve(input)?;
        if let Ok(m) = std::fs::symlink_metadata(&path) {
            if m.file_type().is_symlink() || m.is_dir() {
                return Err(Error::Denied(format!("{} is not a regular file", self.rel(&path))));
            }
        }
        let parent = path.parent().ok_or_else(|| self.escape(input))?;
        std::fs::create_dir_all(parent)?;
        // Re-check after creating directories.
        self.resolve(input)?;
        let tmp = parent.join(format!(".hatch-tmp-{}-{}", std::process::id(), hatch_core::ids::random_id("w")));
        let mut f = OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        if let Ok(old) = std::fs::metadata(&path) {
            let _ = std::fs::set_permissions(&tmp, old.permissions());
        }
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(path)
    }

    /// Remove a regular file (used by checkpoint rewind of a created file).
    pub fn remove_file(&self, input: &str) -> Result<()> {
        let path = self.resolve(input)?;
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => Ok(std::fs::remove_file(&path)?),
            Ok(_) => Err(Error::Denied(format!("{} is not a regular file", self.rel(&path)))),
            Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn ws() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        let w = Workspace::new(d.path()).unwrap();
        (d, w)
    }

    #[test]
    fn resolves_inside_and_refuses_escapes() {
        let (_d, w) = ws();
        assert_eq!(w.resolve("a/b.txt").unwrap(), w.root().join("a/b.txt"));
        assert_eq!(w.resolve("./a/../b.txt").unwrap(), w.root().join("b.txt"));
        assert!(w.resolve("../outside").is_err());
        assert!(w.resolve("a/../../x").is_err());
        assert!(w.resolve("/etc/passwd").is_err());
        assert!(w.resolve(&w.root().join("ok").display().to_string()).is_ok());
        assert!(Workspace::new(Path::new("/")).is_err());
    }

    #[test]
    fn symlinks_pointing_outside_are_refused() {
        let (_d, w) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "CANARY").unwrap();
        symlink(outside.path(), w.root().join("link")).unwrap();
        assert!(w.resolve("link/secret").is_err());
        symlink(outside.path().join("secret"), w.root().join("file-link")).unwrap();
        assert!(w.read_bytes("file-link", 100).is_err());
        assert!(w.write_atomic("file-link", b"x").is_err());
        assert_eq!(std::fs::read_to_string(outside.path().join("secret")).unwrap(), "CANARY");
        // Links that stay inside are fine to traverse.
        std::fs::create_dir(w.root().join("real")).unwrap();
        symlink(w.root().join("real"), w.root().join("inner")).unwrap();
        assert!(w.resolve("inner/x").is_ok());
    }

    #[test]
    fn atomic_write_and_read_back() {
        let (_d, w) = ws();
        w.write_atomic("dir/new.txt", "你好".as_bytes()).unwrap();
        assert_eq!(w.read_bytes("dir/new.txt", 100).unwrap().unwrap(), "你好".as_bytes());
        assert!(w.read_bytes("missing", 10).unwrap().is_none());
        assert!(w.read_bytes("dir/new.txt", 2).is_err(), "size limit");
        w.remove_file("dir/new.txt").unwrap();
        assert!(!w.root().join("dir/new.txt").exists());
    }
}
