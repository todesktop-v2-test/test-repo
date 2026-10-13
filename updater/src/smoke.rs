use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::identity::BuildIdentity;

/// ToDesktop's smoke test drives an app on a fresh machine: it installs
/// the release users have now, updates it to the build under test, and
/// checks each launch. The app reports what its updater does to the
/// test's local server (TODESKTOP_SMOKE_TEST_API_URL), like
/// @todesktop/runtime's initSmokeTest does for v1's Electron apps.
///
/// Smoke mode is on only when the test sets TODESKTOP_SMOKE_TEST=1. It
/// changes when the updater checks and installs (soon, and at once), and
/// with TODESKTOP_SMOKE_TEST_REINSTALL=1 it reinstalls an offer of the
/// build it already runs. It never changes what the updater trusts: every
/// offer is still verified against the app's keys.
pub struct SmokeReporter {
    agent: ureq::Agent,
    url: String,
}

/// What smoke mode changes, read once from the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmokeMode {
    pub enabled: bool,
    /// Seconds between checks while under test.
    pub check_seconds: u64,
    /// Accept an offer of the running build ("Next update": the build's
    /// own updater downloads, verifies, swaps and relaunches it).
    pub reinstall: bool,
}

impl SmokeMode {
    pub fn from_env() -> Self {
        let on =
            |name: &str| std::env::var(name).is_ok_and(|value| value == "1" || value == "true");
        if !on("TODESKTOP_SMOKE_TEST") {
            return Self::default();
        }
        Self {
            enabled: true,
            check_seconds: std::env::var("TODESKTOP_SMOKE_TEST_CHECK_SECONDS")
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|seconds| *seconds > 0)
                .unwrap_or(2),
            reinstall: on("TODESKTOP_SMOKE_TEST_REINSTALL"),
        }
    }

    pub fn current() -> &'static SmokeMode {
        static MODE: OnceLock<SmokeMode> = OnceLock::new();
        MODE.get_or_init(Self::from_env)
    }
}

impl SmokeReporter {
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("TODESKTOP_SMOKE_TEST_API_URL").ok()?;
        if url.is_empty() {
            return None;
        }
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build();
        Some(Self { agent, url })
    }

    pub fn report(&self, event: &str, identity: &BuildIdentity, data: serde_json::Value) {
        let _ = self
            .agent
            .post(&self.url)
            .send_json(body(event, identity, data));
    }
}

fn body(event: &str, identity: &BuildIdentity, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "event": event,
        "appId": identity.app_id,
        "buildId": identity.build_id,
        "buildNumber": identity.build_number,
        "version": identity.version,
        "installKind": identity.install_kind,
        "pid": std::process::id(),
        "at": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64),
        "data": data,
    })
}

/// Under a smoke test, reports a panic (message and where) before the
/// process dies, so the test can say why the app crashed. Installed once;
/// the previous hook still runs.
pub fn report_panics(identity: &BuildIdentity) {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return;
    }
    let Some(reporter) = SmokeReporter::from_env() else {
        return;
    };
    let identity = identity.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".to_owned());
        let location = info
            .location()
            .map(|at| format!("{}:{}", at.file(), at.line()));
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        reporter.report(
            "crashed",
            &identity,
            serde_json::json!({ "message": message, "location": location, "thread": thread }),
        );
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_carry_the_build_and_process() {
        let identity: BuildIdentity = serde_json::from_value(serde_json::json!({
            "appId": "acme", "buildId": "b3", "buildNumber": 3, "version": "1.2.0",
            "executable": "acme", "installKind": "tar.gz", "updateBaseUrl": "http://edge", "updateKeys": []
        }))
        .unwrap();
        let event = body("healthy", &identity, serde_json::json!({}));
        assert_eq!(event["event"], "healthy");
        assert_eq!(event["buildNumber"], 3);
        assert_eq!(event["version"], "1.2.0");
        assert_eq!(event["pid"], std::process::id());
        assert!(event["at"].as_u64().unwrap() > 1_700_000_000_000);
    }
}
