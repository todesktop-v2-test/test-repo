//! Updates as an app sees them: a policy runs the lifecycle (check,
//! download, install) and the app observes one state and calls a few
//! actions. The Tauri plugin exposes exactly this to JavaScript; a daemon
//! can use it directly.
//!
//! The policy comes from the app's todesktop config, through the build's
//! identity. Each step is automatic or manual on its own; with
//! everything manual the app drives every step. Urgent releases (required
//! updates and rollbacks) are still looked for daily unless turned off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Offer, PendingUpdate, Result, SmokeMode, Updater};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateDownload {
    Automatic,
    Manual,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateInstall {
    /// Apply a downloaded update when the app quits.
    OnQuit,
    /// Apply and relaunch as soon as it's downloaded (daemons, tray agents).
    Immediately,
    /// Wait for restart() or install_on_quit().
    Manual,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateUrgent {
    CheckDaily,
    Off,
}

/// As the runner writes it into todesktop-build.json.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePolicy {
    /// None: only when the app calls check().
    pub check_every_seconds: Option<u64>,
    pub download: UpdateDownload,
    pub install: UpdateInstall,
    pub urgent: UpdateUrgent,
    /// How long a launch must run to count as healthy without the app
    /// calling health_ready(). None: a minute.
    #[serde(default)]
    pub health_seconds: Option<u64>,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            check_every_seconds: Some(3600),
            download: UpdateDownload::Automatic,
            install: UpdateInstall::OnQuit,
            urgent: UpdateUrgent::CheckDaily,
            health_seconds: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub build_number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
    pub size_bytes: u64,
    pub required: bool,
    pub rollback: bool,
}

impl UpdateInfo {
    fn of(offer: &Offer) -> Self {
        Self {
            version: offer.record.version.clone(),
            build_number: offer.record.build_number,
            notes: offer.notes.clone(),
            released_at: offer.record.created_at.clone(),
            size_bytes: offer.file.size,
            required: offer.required,
            rollback: offer.rollback_id.is_some(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum UpdateState {
    #[serde(rename_all = "camelCase")]
    Idle {
        #[serde(skip_serializing_if = "Option::is_none")]
        last_checked_at: Option<String>,
    },
    Checking,
    /// Only with manual downloads, or an urgent release found by the daily check.
    Available {
        update: UpdateInfo,
    },
    /// `progress` from 0 to 1.
    Downloading {
        update: UpdateInfo,
        progress: f64,
    },
    /// Downloaded and verified; applies on restart (or quit).
    Ready {
        update: UpdateInfo,
    },
    #[serde(rename_all = "camelCase")]
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_at: Option<String>,
    },
    /// A package manager (apt, dnf, pacman) updates this install.
    Managed {
        by: String,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstallInfo {
    pub version: String,
    pub build_number: u64,
    pub build_id: String,
    pub install_id: Option<String>,
    pub install_kind: String,
    pub channel: Option<String>,
}

type Listener = Box<dyn Fn(&UpdateState) + Send + Sync>;
type Relaunch = Box<dyn Fn() + Send + Sync>;

struct Inner {
    updater: Updater,
    policy: UpdatePolicy,
    state: Mutex<UpdateState>,
    offer: Mutex<Option<Offer>>,
    pending: Mutex<Option<PendingUpdate>>,
    install_on_quit: AtomicBool,
    listeners: Mutex<Vec<Listener>>,
    relaunch: Mutex<Option<Relaunch>>,
    /// This launch was reported healthy (health_ready() or the period).
    healthy: AtomicBool,
    /// One check or download at a time.
    work: Mutex<()>,
}

#[derive(Clone)]
pub struct Updates {
    inner: Arc<Inner>,
}

const DAY: u64 = 24 * 60 * 60;

impl Updates {
    /// With the policy the build carries, or the defaults.
    pub fn new(updater: Updater) -> Self {
        let policy = updater.identity.updates.clone().unwrap_or_default();
        Self::with_policy(updater, policy)
    }

    pub fn with_policy(updater: Updater, mut policy: UpdatePolicy) -> Self {
        // Under a smoke test: check soon and often, install at once.
        let smoke = SmokeMode::current();
        if smoke.enabled {
            policy.check_every_seconds = Some(smoke.check_seconds);
            policy.download = UpdateDownload::Automatic;
            policy.install = UpdateInstall::Immediately;
        }
        let state = match managed_by(&updater) {
            Some(by) => UpdateState::Managed { by },
            None => UpdateState::Idle {
                last_checked_at: None,
            },
        };
        Self {
            inner: Arc::new(Inner {
                updater,
                policy,
                state: Mutex::new(state),
                offer: Mutex::new(None),
                pending: Mutex::new(None),
                install_on_quit: AtomicBool::new(false),
                listeners: Mutex::new(Vec::new()),
                relaunch: Mutex::new(None),
                healthy: AtomicBool::new(false),
                work: Mutex::new(()),
            }),
        }
    }

    /// How restart() starts the new version. Without one, the process
    /// exits for its supervisor (launchd, systemd) to start it.
    pub fn set_relaunch(&self, relaunch: impl Fn() + Send + Sync + 'static) {
        *lock(&self.inner.relaunch) = Some(Box::new(relaunch));
    }

    /// For apps nothing supervises (a desktop app the user started):
    /// restart() starts the new version itself, with this process's
    /// arguments and environment, then exits.
    pub fn relaunch_by_spawning(&self) {
        let updates = self.clone();
        self.set_relaunch(move || {
            let updater = &updates.inner.updater;
            match updater.launch_command().map(|mut command| command.spawn()) {
                Some(Ok(child)) => {
                    updater.report("relaunching", serde_json::json!({ "pid": child.id() }));
                }
                Some(Err(err)) => {
                    eprintln!("todesktop-updater: cannot start the new version: {err}");
                    updater.report(
                        "relaunch-failed",
                        serde_json::json!({ "error": err.to_string() }),
                    );
                }
                None => eprintln!("todesktop-updater: nothing to relaunch"),
            }
            std::process::exit(0);
        });
    }

    pub fn policy(&self) -> &UpdatePolicy {
        &self.inner.policy
    }

    pub fn updater(&self) -> &Updater {
        &self.inner.updater
    }

    pub fn state(&self) -> UpdateState {
        lock(&self.inner.state).clone()
    }

    pub fn on_change(&self, listener: impl Fn(&UpdateState) + Send + Sync + 'static) {
        lock(&self.inner.listeners).push(Box::new(listener));
    }

    /// Runs the policy in the background: checks on its schedule (or
    /// daily for urgent releases only), downloads and installs as it says.
    pub fn start(&self) {
        if self.is_managed() {
            return;
        }
        self.report_launch();
        let policy = &self.inner.policy;
        let (every, urgent_only) = match (policy.check_every_seconds, policy.urgent) {
            (Some(seconds), _) => (seconds.max(1), false),
            (None, UpdateUrgent::CheckDaily) => (DAY, true),
            (None, UpdateUrgent::Off) => return,
        };
        let updates = self.clone();
        std::thread::spawn(move || loop {
            if lock(&updates.inner.pending).is_none() {
                let state = updates.check_now(urgent_only, every);
                let automatic = updates.inner.policy.download == UpdateDownload::Automatic;
                if matches!(state, UpdateState::Available { .. }) && automatic && !urgent_only {
                    if let Err(err) = updates.download() {
                        eprintln!("todesktop-updater: download failed: {err}");
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(every));
        });
    }

    /// Checks now. With automatic downloads, a found update starts
    /// downloading in the background; the state follows.
    pub fn check(&self) -> UpdateState {
        if self.is_managed() {
            return self.state();
        }
        let every = self.inner.policy.check_every_seconds.unwrap_or(DAY);
        let state = self.check_now(false, every);
        if matches!(state, UpdateState::Available { .. })
            && self.inner.policy.download == UpdateDownload::Automatic
        {
            let updates = self.clone();
            std::thread::spawn(move || {
                if let Err(err) = updates.download() {
                    eprintln!("todesktop-updater: download failed: {err}");
                }
            });
        }
        state
    }

    fn check_now(&self, urgent_only: bool, retry_seconds: u64) -> UpdateState {
        let _work = lock(&self.inner.work);
        if let Some(pending) = lock(&self.inner.pending).as_ref() {
            return UpdateState::Ready {
                update: UpdateInfo::of(&pending.offer),
            };
        }
        let before = self.state();
        self.set(UpdateState::Checking);
        let state = match self.inner.updater.check() {
            Ok(Some(offer)) if !urgent_only || offer.required || offer.rollback_id.is_some() => {
                let update = UpdateInfo::of(&offer);
                *lock(&self.inner.offer) = Some(offer);
                UpdateState::Available { update }
            }
            // The daily urgent check found an ordinary update: say nothing.
            Ok(Some(_)) => before,
            Ok(None) => UpdateState::Idle {
                last_checked_at: Some(rfc3339(SystemTime::now())),
            },
            Err(err) => UpdateState::Error {
                message: err.to_string(),
                retry_at: Some(rfc3339(
                    SystemTime::now() + Duration::from_secs(retry_seconds),
                )),
            },
        };
        self.set(state.clone());
        state
    }

    /// Downloads and verifies the update found by the last check, then
    /// installs it as the policy says: now, at quit, or when asked.
    pub fn download(&self) -> Result<UpdateState> {
        let work = lock(&self.inner.work);
        if let Some(pending) = lock(&self.inner.pending).as_ref() {
            return Ok(UpdateState::Ready {
                update: UpdateInfo::of(&pending.offer),
            });
        }
        let offer = lock(&self.inner.offer)
            .clone()
            .ok_or("no update to download; check first")?;
        let update = UpdateInfo::of(&offer);
        self.set(UpdateState::Downloading {
            update: update.clone(),
            progress: 0.0,
        });
        let mut reported = 0.0;
        let downloaded = self.inner.updater.download(&offer, |read, total| {
            let progress = (read as f64 / total.max(1) as f64 * 100.0).floor() / 100.0;
            if progress > reported {
                reported = progress;
                self.set(UpdateState::Downloading {
                    update: update.clone(),
                    progress,
                });
            }
        });
        let pending = match downloaded {
            Ok(pending) => pending,
            Err(err) => {
                self.set(UpdateState::Error {
                    message: err.to_string(),
                    retry_at: None,
                });
                return Err(err);
            }
        };
        *lock(&self.inner.pending) = Some(pending);
        let ready = UpdateState::Ready { update };
        self.set(ready.clone());
        drop(work);
        match self.inner.policy.install {
            UpdateInstall::Immediately => self.restart()?,
            UpdateInstall::OnQuit => self.inner.install_on_quit.store(true, Ordering::SeqCst),
            UpdateInstall::Manual => {}
        }
        Ok(ready)
    }

    /// Applies the ready update and relaunches into it.
    pub fn restart(&self) -> Result<()> {
        let pending = lock(&self.inner.pending)
            .take()
            .ok_or("no update is ready")?;
        if let Err(err) = self.inner.updater.apply(&pending) {
            self.set(UpdateState::Error {
                message: err.to_string(),
                retry_at: None,
            });
            return Err(err);
        }
        match lock(&self.inner.relaunch).as_ref() {
            Some(relaunch) => relaunch(),
            None => {
                self.inner
                    .updater
                    .report("exiting", serde_json::json!({ "for": "update" }));
                std::process::exit(0)
            }
        }
        Ok(())
    }

    /// Applies the ready update when the app quits, instead of now.
    pub fn install_on_quit(&self) -> Result<()> {
        if lock(&self.inner.pending).is_none() {
            return Err("no update is ready".into());
        }
        self.inner.install_on_quit.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// For the app to call as it quits.
    pub fn on_quit(&self) {
        if !self.inner.install_on_quit.load(Ordering::SeqCst) {
            return;
        }
        if let Some(pending) = lock(&self.inner.pending).take() {
            if let Err(err) = self.inner.updater.apply(&pending) {
                eprintln!("todesktop-updater: cannot install at quit: {err}");
            }
        }
    }

    pub fn info(&self) -> InstallInfo {
        let updater = &self.inner.updater;
        InstallInfo {
            version: updater.identity.version.clone(),
            build_number: updater.identity.build_number,
            build_id: updater.identity.build_id.clone(),
            install_id: updater.install_id().map(str::to_owned),
            install_kind: updater.identity.install_kind.clone(),
            channel: updater.cohort(),
        }
    }

    pub fn channel(&self) -> Option<String> {
        self.inner.updater.cohort()
    }

    /// Joins a channel (a cohort such as beta), or stable with None, and
    /// checks again.
    pub fn join_channel(&self, channel: Option<&str>) -> Result<UpdateState> {
        self.inner.updater.set_cohort(channel)?;
        Ok(self.check())
    }

    /// The app says this launch works. Rollout health counts launches
    /// after an update that never say so as failed.
    pub fn health_ready(&self) {
        if self.inner.healthy.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.updater.report("healthy", serde_json::json!({}));
        let updater = &self.inner.updater;
        updater.send_events(&[updater.build_event("healthy")]);
    }

    /// Reports this launch; it counts as healthy once the app calls
    /// health_ready() or has run for the health period (default a minute).
    fn report_launch(&self) {
        let updater = &self.inner.updater;
        updater.send_events(&[updater.build_event("launch")]);
        let period = self.inner.policy.health_seconds.unwrap_or(60);
        let updates = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(period));
            updates.health_ready();
        });
    }

    fn is_managed(&self) -> bool {
        matches!(self.state(), UpdateState::Managed { .. })
    }

    fn set(&self, state: UpdateState) {
        {
            let mut current = lock(&self.inner.state);
            if *current == state {
                return;
            }
            *current = state.clone();
        }
        for listener in lock(&self.inner.listeners).iter() {
            listener(&state);
        }
    }
}

fn managed_by(updater: &Updater) -> Option<String> {
    if updater.can_self_update() {
        return None;
    }
    Some(
        match updater.identity.install_kind.as_str() {
            "deb" => "apt",
            "rpm" => "dnf",
            "pacman" => "pacman",
            other => other,
        }
        .to_owned(),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// UTC, e.g. 2026-10-11T09:30:00Z.
fn rfc3339(time: SystemTime) -> String {
    let seconds = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_791_672_000)),
            "2026-10-10T22:40:00Z"
        );
    }

    #[test]
    fn reads_the_policy_the_runner_writes() {
        let policy: UpdatePolicy = serde_json::from_str(
            r#"{"checkEverySeconds":null,"download":"manual","install":"on-quit","urgent":"off"}"#,
        )
        .unwrap();
        assert_eq!(policy.check_every_seconds, None);
        assert_eq!(policy.download, UpdateDownload::Manual);
        assert_eq!(policy.install, UpdateInstall::OnQuit);
        assert_eq!(policy.urgent, UpdateUrgent::Off);
    }

    #[test]
    fn serializes_state_as_the_javascript_api_types_it() {
        let state = UpdateState::Downloading {
            update: UpdateInfo {
                version: "1.0.2".into(),
                build_number: 3,
                notes: None,
                released_at: None,
                size_bytes: 10,
                required: false,
                rollback: false,
            },
            progress: 0.5,
        };
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            serde_json::json!({
                "status": "downloading",
                "progress": 0.5,
                "update": {
                    "version": "1.0.2", "buildNumber": 3, "sizeBytes": 10,
                    "required": false, "rollback": false
                }
            })
        );
        assert_eq!(
            serde_json::to_value(UpdateState::Idle {
                last_checked_at: None
            })
            .unwrap(),
            serde_json::json!({ "status": "idle" })
        );
    }
}
