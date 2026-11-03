//! Device capability manifest (Muse deep-dive §12; roadmap R13).
//!
//! Evidence (E-behaviour): device capability is declarative — a
//! device description file lists the commands it supports (the
//! deep-dive's example: a Mac currently has no `computer.control`,
//! so it cannot operate its Chrome window); when a capability is
//! missing, the agent says so honestly and tells the user how to
//! enable it, instead of pretending it works.
//!
//! What is Muse's here: the *shape* (declarative manifest, honest
//! missing-capability reply with an enable path). What is ORIGINAL
//! design (marked as such): the file layout
//! (`$HATCH_HOME/devices/<device-id>/capabilities.json`), the exact
//! capability names in the default manifest, and the enable-path
//! texts. The defaults below describe the machine this repo actually
//! runs on — the user's Mac has no `computer.control` (see the
//! workspace `TOOLS.md` note) — and the user edits the manifest when
//! a device gains a capability; the code never claims a capability
//! that is not in the manifest.
//!
//! Not in this phase: real device pairing, `peerd` sync, and any
//! device control (`computer.control`-style capability needs
//! device-side authorisation; this repo has no device). The manifest
//! is the interface a future pairing step would write.

use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One capability a device may have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// Stable name, e.g. `computer.control`, `file_access`.
    pub name: String,
    /// Whether this device currently has it. A `false` entry is a
    /// *known* gap: the device is known not to have it, and
    /// `how_to_enable` says how to close the gap.
    pub supported: bool,
    /// What it lets the agent do, in one line.
    pub description: String,
    /// How the user enables it — shown verbatim when missing.
    /// Empty means "no known enable path": the reply says so.
    #[serde(default)]
    pub how_to_enable: String,
}

/// A device's declared capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityManifest {
    pub device_id: String,
    pub device_name: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

impl CapabilityManifest {
    /// The local machine's default manifest (ORIGINAL: our own
    /// defaults, not Muse's). File access is what the pipe transport
    /// itself does; screen control is the documented gap on the
    /// user's Mac.
    pub fn local_default() -> Self {
        CapabilityManifest {
            device_id: "local".into(),
            device_name: "本机（运行 hatch 的设备）".into(),
            capabilities: vec![
                Capability {
                    name: "file_access".into(),
                    supported: true,
                    description: "读写本地文件（pipe transport 的收件箱/发件箱）".into(),
                    how_to_enable: String::new(),
                },
                Capability {
                    name: "computer.control".into(),
                    supported: false,
                    description: "操作设备上的应用窗口（如 Chrome）".into(),
                    how_to_enable: "在 Mac App 设置里打开控制权限，并在 macOS 系统设置 → 隐私与安全性 → 辅助功能与屏幕录制中授予 hatch 权限。".into(),
                },
                Capability {
                    name: "camera".into(),
                    supported: false,
                    description: "使用设备摄像头".into(),
                    how_to_enable: "在设备系统设置 → 隐私与安全性 → 相机中授予 hatch 权限。".into(),
                },
                Capability {
                    name: "microphone".into(),
                    supported: false,
                    description: "使用设备麦克风".into(),
                    how_to_enable: "在设备系统设置 → 隐私与安全性 → 麦克风中授予 hatch 权限。".into(),
                },
            ],
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.capabilities.iter().any(|c| c.name == name && c.supported)
    }

    /// The declaration of a known-but-absent capability, if the
    /// manifest knows about `name` at all. `None` means either
    /// supported or wholly unknown — check [`Self::supports`] first.
    pub fn missing(&self, name: &str) -> Option<&Capability> {
        self.capabilities.iter().find(|c| c.name == name && !c.supported)
    }

    /// True when the manifest declares `name` as supported.
    pub fn supports(&self, name: &str) -> bool {
        self.has(name)
    }
}

/// The fixed response form for a missing capability: name what is
/// missing, say plainly it cannot be done, and give the enable path.
/// Never pretend it works, never silently skip it.
pub fn missing_reply(device: &CapabilityManifest, name: &str) -> String {
    let enable = device
        .missing(name)
        .map(|c| c.how_to_enable.clone())
        .filter(|s| !s.is_empty());
    match enable {
        Some(path) => format!(
            "设备 {} 不支持 {}，所以我现在做不了这件事。开通路径：{path}开通后告诉我，我再试。",
            device.device_name, name
        ),
        None => format!(
            "设备 {} 不支持 {}，所以我现在做不了这件事，也没有已知的开通路径（可以在 {} 里为这个设备补充该能力的开通说明）。",
            device.device_name,
            name,
            CapabilityStore::manifest_path_for(&device.device_id).display()
        ),
    }
}

/// Manifests under `$HATCH_HOME/devices/<device-id>/capabilities.json`.
pub struct CapabilityStore {
    root: PathBuf,
}

impl CapabilityStore {
    pub fn new(home: &Path) -> Self {
        CapabilityStore { root: home.join("devices") }
    }

    pub fn manifest_path_for(device_id: &str) -> PathBuf {
        PathBuf::from("devices").join(device_id).join("capabilities.json")
    }

    pub fn manifest_path(&self, device_id: &str) -> PathBuf {
        self.root.join(device_id).join("capabilities.json")
    }

    /// Load a manifest; a missing file is an error (unlike discretion,
    /// an unknown device must not silently be treated as capable —
    /// fail closed, and tell the user to declare it).
    pub fn load(&self, device_id: &str) -> Result<CapabilityManifest> {
        if device_id.trim().is_empty()
            || device_id.contains(['/', '\\'])
            || device_id == ".."
        {
            return Err(Error::Invalid(format!("invalid device id `{device_id}`")));
        }
        let path = self.manifest_path(device_id);
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::NotFound(format!("设备 {device_id} 没有能力清单（{}：{e}）；先用 `hatch transport device init {device_id}` 创建", path.display())))?;
        serde_json::from_str(&raw).map_err(|e| Error::Invalid(format!("设备 {device_id} 的能力清单已损坏：{e}")))
    }

    /// The local device's manifest: the stored one, or the default
    /// (written out on first use so the user can see and edit it).
    pub fn local(&self) -> Result<CapabilityManifest> {
        match self.load("local") {
            Ok(m) => Ok(m),
            Err(Error::NotFound(_)) => {
                let m = CapabilityManifest::local_default();
                self.save(&m)?;
                Ok(m)
            }
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, manifest: &CapabilityManifest) -> Result<()> {
        let path = self.manifest_path(&manifest.device_id);
        std::fs::create_dir_all(path.parent().expect("manifest has a parent"))?;
        std::fs::write(&path, serde_json::to_string_pretty(manifest)?)?;
        Ok(())
    }

    /// Device ids that have a manifest.
    pub fn list_devices(&self) -> Result<Vec<String>> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for e in rd.flatten() {
            if e.path().is_dir() && self.manifest_path(&e.file_name().to_string_lossy()).is_file() {
                out.push(e.file_name().to_string_lossy().to_string());
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_capability_reply_names_the_enable_path() {
        let m = CapabilityManifest::local_default();
        assert!(m.supports("file_access"));
        assert!(!m.supports("computer.control"));
        let reply = missing_reply(&m, "computer.control");
        assert!(reply.contains("不支持 computer.control"), "{reply}");
        assert!(reply.contains("做不了"), "{reply}");
        assert!(reply.contains("辅助功能"), "{reply}");
        // Unknown capability: still honest, still no pretence.
        let reply = missing_reply(&m, "teleport");
        assert!(reply.contains("不支持 teleport") && reply.contains("没有已知的开通路径"), "{reply}");
    }

    #[test]
    fn manifest_round_trips_and_unknown_device_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = CapabilityStore::new(dir.path());
        assert!(store.load("macbook").is_err(), "unknown device must not be treated as capable");
        let m = store.local().unwrap();
        assert!(store.manifest_path("local").is_file(), "default manifest is written for the user to edit");
        let back = store.load("local").unwrap();
        assert_eq!(back, m);
        assert_eq!(store.list_devices().unwrap(), vec!["local"]);
    }
}
