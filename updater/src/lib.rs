//! ToDesktop v2 updater (spike).
//!
//! Every artifact the runner produces carries a `todesktop-build.json`
//! next to the app's binary (in `Contents/Resources` of a macOS app). The
//! updater reads it to learn which app and build it is, where to check
//! and which keys to trust. It self-updates tarball and AppImage installs
//! on Linux and app bundles on macOS; deb, rpm and pacman installs are
//! updated by their package manager.

mod app_bundle;
mod identity;
mod install;
mod record;
mod smoke;
mod updates;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub use app_bundle::Swap;
pub use identity::{
    bundle_of, identity_path, BuildIdentity, InstallState, IDENTITY_FILE, INSTALL_STATE_FILE,
};
pub use record::{
    authorize_downgrade, verify_record, ArtifactFile, BuildRecord, RollbackStatement,
    SignedBuildRecord, SignedRollbackStatement,
};
pub use smoke::SmokeReporter;
pub use updates::{
    InstallInfo, UpdateDownload, UpdateInfo, UpdateInstall, UpdatePolicy, UpdateState,
    UpdateUrgent, Updates,
};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// An update the edge offered and whose record verified.
#[derive(Debug, Clone)]
pub struct Offer {
    pub file: ArtifactFile,
    pub record: BuildRecord,
    /// Released with --required: older builds must update.
    pub required: bool,
    /// Set when the offer is a signed rollback to a lower build.
    pub rollback_id: Option<String>,
    pub rollout_id: Option<String>,
    /// Where the edge says to fetch `file`. What arrives is checked
    /// against the signed record, wherever it came from.
    pub url: String,
}

/// An offer whose artifact is downloaded and verified against the signed
/// record, ready to apply.
#[derive(Debug, Clone)]
pub struct PendingUpdate {
    pub archive: PathBuf,
    pub offer: Offer,
}

/// What the updater replaces, and how.
#[derive(Debug, Clone)]
pub enum Layout {
    /// Linux: `<root>/versions/<buildId>/` plus a `current` link.
    Tarball { root: PathBuf },
    /// macOS: an `.app`, replaced whole from the build's zip.
    AppBundle { bundle: PathBuf },
    /// Linux: a single `.AppImage` file (the runtime's APPIMAGE), replaced
    /// by the new one.
    AppImage { file: PathBuf },
}

impl Layout {
    /// The artifact format updates come in.
    pub fn update_format(&self) -> &'static str {
        match self {
            Layout::Tarball { .. } => "tar.gz",
            Layout::AppBundle { .. } => "zip",
            Layout::AppImage { .. } => "appimage",
        }
    }
}

/// An install this updater keeps current.
#[derive(Debug, Clone)]
pub struct SelfInstall {
    /// Where update downloads go.
    pub cache_dir: PathBuf,
    pub layout: Layout,
    pub state: InstallState,
    /// Where install.json lives.
    pub state_dir: PathBuf,
}

pub struct Updater {
    /// The version directory (Linux) or the app bundle (macOS).
    pub app_dir: PathBuf,
    pub identity: BuildIdentity,
    pub install: Option<SelfInstall>,
    agent: ureq::Agent,
    /// The install's cohort: from install.json or the identity at start,
    /// then whatever the edge assigns (see stickyCohort()) or the app joins.
    cohort: Mutex<Option<String>>,
    os_version: Option<String>,
    smoke: Option<SmokeReporter>,
}

pub fn current_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    }
}

/// macOS's product version, e.g. "15.4", so the edge never offers a
/// build whose minimum system version is higher. None elsewhere.
pub fn current_os_version() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    let version = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !version.is_empty()).then_some(version)
}

pub fn current_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x64",
    }
}

impl Updater {
    /// For the running app: the app bundle its binary is in, or else the
    /// directory of its real binary.
    pub fn from_current_exe() -> Result<Self> {
        let exe = std::env::current_exe()?.canonicalize()?;
        match bundle_of(&exe) {
            Some(bundle) => Self::from_app_dir(&bundle),
            None => Self::from_app_dir(exe.parent().ok_or("binary has no parent directory")?),
        }
    }

    pub fn from_app_dir(app_dir: &Path) -> Result<Self> {
        let identity = BuildIdentity::load(app_dir)?;
        let install = match identity.install_kind.as_str() {
            "tar.gz" => install::tarball_root(app_dir).map(|root| SelfInstall {
                cache_dir: root.join("downloads"),
                state: InstallState::load(&root).unwrap_or_default(),
                state_dir: root.clone(),
                layout: Layout::Tarball { root },
            }),
            // Installed from the DMG, or replaced from a zip by an update.
            "dmg" | "zip" if identity::is_app_bundle(app_dir) => {
                let library = home_dir()?.join("Library");
                let state_dir = library.join("Application Support").join(&identity.app_id);
                Some(SelfInstall {
                    cache_dir: library.join("Caches").join(&identity.app_id),
                    state: InstallState::load_or_create(&state_dir)?,
                    state_dir,
                    layout: Layout::AppBundle {
                        bundle: app_dir.to_path_buf(),
                    },
                })
            }
            // The AppImage runtime names the file it runs from. Run from an
            // extracted AppDir there is no file to replace.
            "appimage" => match std::env::var_os("APPIMAGE").filter(|file| !file.is_empty()) {
                Some(file) => {
                    let home = home_dir()?;
                    let xdg = |name: &str, fallback: &[&str]| {
                        std::env::var_os(name)
                            .filter(|dir| !dir.is_empty())
                            .map(PathBuf::from)
                            .unwrap_or_else(|| {
                                fallback
                                    .iter()
                                    .fold(home.clone(), |dir, part| dir.join(part))
                            })
                    };
                    let state_dir =
                        xdg("XDG_DATA_HOME", &[".local", "share"]).join(&identity.app_id);
                    Some(SelfInstall {
                        cache_dir: xdg("XDG_CACHE_HOME", &[".cache"]).join(&identity.app_id),
                        state: InstallState::load_or_create(&state_dir)?,
                        state_dir,
                        layout: Layout::AppImage {
                            file: PathBuf::from(file),
                        },
                    })
                }
                None => None,
            },
            _ => None,
        };
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .build();
        let smoke = SmokeReporter::from_env();
        let cohort = install
            .as_ref()
            .and_then(|install| install.state.cohort.clone())
            .or_else(|| identity.cohort.clone());
        let updater = Self {
            app_dir: app_dir.to_path_buf(),
            identity,
            install,
            agent,
            cohort: Mutex::new(cohort),
            os_version: current_os_version(),
            smoke,
        };
        updater.report("started", serde_json::json!({}));
        Ok(updater)
    }

    /// Package-manager installs return false: apt, dnf or pacman own them.
    pub fn can_self_update(&self) -> bool {
        self.install.is_some()
    }

    pub fn cohort(&self) -> Option<String> {
        self.cohort.lock().ok().and_then(|cohort| cohort.clone())
    }

    pub fn install_id(&self) -> Option<&str> {
        Some(self.install.as_ref()?.state.install_id.as_str())
    }

    /// Joins a cohort (a channel such as beta), or leaves it with None,
    /// for this run and the next.
    pub fn set_cohort(&self, cohort: Option<&str>) -> Result<()> {
        let install = self.install.as_ref().ok_or("not a self-updating install")?;
        if let Some(cohort) = cohort {
            if !valid_cohort(cohort) {
                return Err(format!("not a cohort name: {cohort}").into());
            }
        }
        InstallState {
            cohort: cohort.map(str::to_owned),
            install_id: install.state.install_id.clone(),
        }
        .save(&install.state_dir)?;
        if let Ok(mut current) = self.cohort.lock() {
            *current = cohort.map(str::to_owned);
        }
        Ok(())
    }

    /// Joins the cohort the edge assigned, unless the install has one.
    fn assign_cohort(&self, cohort: &str) {
        if !valid_cohort(cohort) || self.cohort().is_some() {
            return;
        }
        if let Err(err) = self.set_cohort(Some(cohort)) {
            eprintln!("todesktop-updater: cannot save cohort {cohort}: {err}");
        }
        self.report("cohort-assigned", serde_json::json!({ "cohort": cohort }));
    }

    pub(crate) fn report(&self, event: &str, data: serde_json::Value) {
        if let Some(smoke) = &self.smoke {
            smoke.report(event, &self.identity, data);
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/v2/{}/{}",
            self.identity.update_base_url.trim_end_matches('/'),
            self.identity.app_id,
            path
        )
    }

    /// Asks the edge whether this install should move to another build.
    pub fn check(&self) -> Result<Option<Offer>> {
        let install = self.install.as_ref().ok_or("not a self-updating install")?;
        let mut request = self
            .agent
            .get(&self.url("update"))
            .set("x-td-platform", current_platform())
            .set("x-td-arch", current_arch())
            .set(
                "x-td-current-build-number",
                &self.identity.build_number.to_string(),
            )
            .set("x-td-install-id", &install.state.install_id);
        if let Some(version) = &self.os_version {
            request = request.set("x-td-os-version", version);
        }
        if let Some(cohort) = self.cohort() {
            request = request.set("x-td-cohorts", &cohort);
        }
        let body: serde_json::Value = request.call()?.into_json()?;
        if let Some(cohort) = body["cohort"].as_str() {
            self.assign_cohort(cohort);
        }
        if body["update"] != serde_json::Value::Bool(true) {
            self.report("checked", serde_json::json!({ "update": false }));
            return Ok(None);
        }

        let signed: SignedBuildRecord = serde_json::from_value(body["signedRecord"].clone())?;
        let record = verify_record(&signed, &self.identity.update_keys)?;
        if record.app_id != self.identity.app_id {
            return Err("record is for another app".into());
        }
        // A lower build is only accepted with a signed rollback statement
        // that covers exactly this install's build.
        let rollback_id = if record.build_number > self.identity.build_number {
            None
        } else {
            let signed: SignedRollbackStatement = serde_json::from_value(body["rollback"].clone())
                .map_err(|_| "offered build is not newer and has no rollback statement")?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let statement = authorize_downgrade(
                &signed,
                &self.identity.update_keys,
                &record,
                self.identity.build_number,
                now,
            )?;
            Some(statement.id)
        };
        let format = install.layout.update_format();
        let file = record
            .files
            .iter()
            .find(|file| {
                file.platform == current_platform()
                    && file.format == format
                    && (file.arch == current_arch() || file.arch == "universal")
            })
            .cloned()
            .ok_or_else(|| format!("offered build has no {format} for this platform"))?;
        let url = body["downloads"][&file.sha256]
            .as_str()
            .ok_or("offer has no download URL for its file")?
            .to_owned();
        let rollout_id = body["rolloutId"].as_str().map(str::to_owned);
        self.report(
            "checked",
            serde_json::json!({
                "update": true,
                "buildNumber": record.build_number,
                "rollbackId": rollback_id,
                "rolloutId": rollout_id,
            }),
        );
        Ok(Some(Offer {
            file,
            record,
            required: body["required"] == serde_json::Value::Bool(true),
            rollback_id,
            rollout_id,
            url,
        }))
    }

    /// Downloads the offered artifact and checks it against the signed
    /// record. `progress` gets (bytes so far, total) as it goes.
    pub fn download(
        &self,
        offer: &Offer,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<PendingUpdate> {
        let install = self.install.as_ref().ok_or("not a self-updating install")?;
        std::fs::create_dir_all(&install.cache_dir)?;
        let archive = install.cache_dir.join(&offer.file.name);

        let response = self.agent.get(&offer.url).call()?;
        let total = offer.file.size;
        let mut reader = install::Progress::new(response.into_reader().take(total + 1), |read| {
            progress(read.min(total), total)
        });
        let actual = install::download_to(&mut reader, &archive)?;
        if actual.size != offer.file.size || actual.sha256 != offer.file.sha256 {
            std::fs::remove_file(&archive).ok();
            return Err(format!(
                "download does not match the signed record (sha256 {} size {})",
                actual.sha256, actual.size
            )
            .into());
        }
        self.report("downloaded", serde_json::json!({ "sha256": actual.sha256 }));
        Ok(PendingUpdate {
            archive,
            offer: offer.clone(),
        })
    }

    /// Downloads, verifies and switches to the offered build. The caller
    /// then exits so its supervisor (systemd or launchd) starts the new
    /// version.
    pub fn install(&self, offer: &Offer) -> Result<PathBuf> {
        let pending = self.download(offer, |_, _| {})?;
        self.apply(&pending)
    }

    /// Switches to a downloaded build: a new version directory (Linux) or
    /// the app bundle swapped in place (macOS). The running process keeps
    /// the old code until it restarts.
    pub fn apply(&self, pending: &PendingUpdate) -> Result<PathBuf> {
        let install = self.install.as_ref().ok_or("not a self-updating install")?;
        let PendingUpdate { archive, offer } = pending;
        let archive = archive.clone();
        let applied = match &install.layout {
            Layout::Tarball { root } => {
                install::apply_tarball(root, &archive, &offer.record).map(|dir| (dir, None))
            }
            Layout::AppBundle { bundle } => {
                app_bundle::apply_app_bundle(bundle, &archive, &offer.record)
                    .map(|swap| (bundle.clone(), Some(swap)))
            }
            Layout::AppImage { file } => {
                install::apply_appimage(file, &archive).map(|file| (file, None))
            }
        };
        std::fs::remove_file(&archive).ok();
        let (dir, swap) = applied?;
        self.report(
            "installed",
            serde_json::json!({
                "buildNumber": offer.record.build_number,
                "swap": swap.map(Swap::as_str),
            }),
        );
        Ok(dir)
    }
}

fn valid_cohort(cohort: &str) -> bool {
    !cohort.is_empty()
        && cohort.len() <= 64
        && cohort
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}
