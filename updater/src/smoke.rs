use std::time::Duration;

use crate::identity::BuildIdentity;

/// Reports updater events to a smoke test, like @todesktop/runtime's
/// initSmokeTest does for Electron apps. Only active when the smoke
/// test sets TODESKTOP_SMOKE_TEST_API_URL; it never changes what the
/// updater trusts or installs.
pub struct SmokeReporter {
    agent: ureq::Agent,
    url: String,
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
        let body = serde_json::json!({
            "event": event,
            "appId": identity.app_id,
            "buildId": identity.build_id,
            "buildNumber": identity.build_number,
            "pid": std::process::id(),
            "data": data,
        });
        // Best effort: a smoke server that is down must not break updates.
        let _ = self.agent.post(&self.url).send_json(body);
    }
}
