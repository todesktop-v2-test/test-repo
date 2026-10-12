use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

pub const IDENTITY_FILE: &str = "todesktop-build.json";
pub const INSTALL_STATE_FILE: &str = "install.json";

/// Written by the runner into every artifact. See BuildIdentity in
/// v2/contracts/src/types.ts.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildIdentity {
    pub app_id: String,
    pub build_id: String,
    pub build_number: u64,
    #[serde(default)]
    pub cohort: Option<String>,
    pub executable: String,
    pub install_kind: String,
    pub update_base_url: String,
    pub update_keys: Vec<String>,
    /// From the app's todesktop config; defaults apply when absent.
    #[serde(default)]
    pub updates: Option<crate::UpdatePolicy>,
    pub version: String,
}

impl BuildIdentity {
    /// `app_dir` is a directory holding the identity next to the binary
    /// (Linux), or a macOS app bundle, which keeps it in
    /// `Contents/Resources` because `Contents/MacOS` may only hold code.
    pub fn load(app_dir: &Path) -> Result<Self> {
        let path = identity_path(app_dir);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }
}

pub fn identity_path(app_dir: &Path) -> PathBuf {
    if is_app_bundle(app_dir) {
        app_dir
            .join("Contents")
            .join("Resources")
            .join(IDENTITY_FILE)
    } else {
        app_dir.join(IDENTITY_FILE)
    }
}

pub fn is_app_bundle(dir: &Path) -> bool {
    dir.extension().is_some_and(|ext| ext == "app") && dir.join("Contents").is_dir()
}

/// `X.app/Contents/MacOS/<binary>` → `X.app`, for the bundle's own
/// binaries. None for anything else, e.g. a Linux install.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

/// Per-install state kept outside what updates replace, so it survives
/// them: next to `versions/` for tarballs (written by install.sh), and in
/// `~/Library/Application Support/<appId>` for app bundles, which are
/// signed and replaced whole.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallState {
    #[serde(default)]
    pub cohort: Option<String>,
    pub install_id: String,
}

impl InstallState {
    pub fn load(dir: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(dir.join(INSTALL_STATE_FILE))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Nothing writes install.json for a DMG install, so the first run
    /// does, with a fresh install ID and no cohort.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        if dir.join(INSTALL_STATE_FILE).exists() {
            return Self::load(dir);
        }
        let state = Self {
            cohort: None,
            install_id: uuid::Uuid::new_v4().to_string(),
        };
        state.save(dir)?;
        Ok(state)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let partial = dir.join(format!("{INSTALL_STATE_FILE}.partial"));
        std::fs::write(&partial, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&partial, dir.join(INSTALL_STATE_FILE))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "td-identity-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_the_bundle_of_its_own_binaries_only() {
        let exe = Path::new("/Applications/Acme Agent.app/Contents/MacOS/acme-agent");
        assert_eq!(
            bundle_of(exe).as_deref(),
            Some(Path::new("/Applications/Acme Agent.app"))
        );
        // A tarball install, a nested non-code path, a folder called MacOS.
        for other in [
            "/home/u/.local/share/acme-agent/versions/b1/acme-agent",
            "/Applications/Acme Agent.app/Contents/Resources/acme-agent",
            "/tmp/Contents/MacOS/acme-agent",
        ] {
            assert_eq!(bundle_of(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn reads_the_identity_from_resources_in_a_bundle() {
        let dir = temp_dir("bundle");
        let bundle = dir.join("Acme Agent.app");
        let resources = bundle.join("Contents").join("Resources");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(
            resources.join(IDENTITY_FILE),
            r#"{"appId":"acme-agent","buildId":"b1","buildNumber":1,"executable":"acme-agent","installKind":"dmg","updateBaseUrl":"http://x","updateKeys":[],"version":"1.0.0"}"#,
        )
        .unwrap();
        let identity = BuildIdentity::load(&bundle).unwrap();
        assert_eq!(identity.install_kind, "dmg");
        // A plain directory reads it next to the binary instead.
        assert!(BuildIdentity::load(&resources.join("..")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn creates_install_state_once() {
        let dir = temp_dir("state")
            .join("Application Support")
            .join("acme-agent");
        let first = InstallState::load_or_create(&dir).unwrap();
        let second = InstallState::load_or_create(&dir).unwrap();
        assert_eq!(first.install_id.len(), 36);
        assert_eq!(first.install_id, second.install_id);
        assert_eq!(second.cohort, None);
        std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).unwrap();
    }
}
