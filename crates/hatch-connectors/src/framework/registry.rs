//! Connector registry: load manifests from a directory and resolve the
//! environment a connector needs.
//!
//! The connectors directory is `$HATCH_HOME/connectors` by default, or the
//! directory named by the `HATCH_CONNECTORS_DIR` environment variable.
//! Each `*.toml` file in it is one connector manifest; a file that fails
//! to parse is reported and skipped, never fatal to the others.

use crate::framework::manifest::ConnectorManifest;
use hatch_core::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Default connectors directory under a home directory.
pub fn default_dir(home: &Path) -> PathBuf {
    match std::env::var("HATCH_CONNECTORS_DIR") {
        Ok(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => home.join("connectors"),
    }
}

/// All manifests found in `dir`, plus one warning per file that could not
/// be parsed. A missing directory is not an error — it means no connectors
/// are installed.
pub fn load_dir(dir: &Path) -> (Vec<ConnectorManifest>, Vec<String>) {
    let mut manifests = Vec::new();
    let mut warnings = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (manifests, warnings),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(text) => match ConnectorManifest::parse(&text) {
                Ok(m) => manifests.push(m),
                Err(e) => warnings.push(format!("{}：{e}", path.display())),
            },
            Err(e) => warnings.push(format!("{}：无法读取：{e}", path.display())),
        }
    }
    manifests.sort_by(|a, b| a.name.cmp(&b.name));
    (manifests, warnings)
}

/// Find one manifest by name in `dir`.
pub fn find(dir: &Path, name: &str) -> Result<ConnectorManifest> {
    let (manifests, warnings) = load_dir(dir);
    for w in warnings {
        eprintln!("（连接器清单警告：{w}）");
    }
    manifests
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| Error::NotFound(format!("no connector named `{name}` in {}", dir.display())))
}

/// Resolve the env map for a manifest from the process environment.
/// Returns the map plus the list of declared vars that are missing.
pub fn resolve_env(
    manifest: &ConnectorManifest,
) -> (HashMap<String, String>, Vec<String>) {
    let mut map = HashMap::new();
    let mut missing = Vec::new();
    for name in &manifest.env {
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => {
                map.insert(name.clone(), v);
            }
            _ => missing.push(name.clone()),
        }
    }
    (map, missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(format!("{name}.toml")), text).unwrap();
    }

    #[test]
    fn load_dir_reads_and_skips_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(
            dir.path(),
            "good",
            "name = \"good\"\ncommand = \"true\"\n[[ops]]\nname = \"status\"\n",
        );
        write_manifest(dir.path(), "bad", "name = \nbroken toml [[[\n");
        let (manifests, warnings) = load_dir(dir.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name, "good");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn load_dir_missing_is_empty() {
        let (m, w) = load_dir(Path::new("/nonexistent/definitely-not-a-dir"));
        assert!(m.is_empty());
        assert!(w.is_empty());
    }

    #[test]
    fn find_missing_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find(dir.path(), "nope").is_err());
    }
}
