//! Runs the agent under launchd, the macOS counterpart of the systemd
//! unit the Linux packages install. KeepAlive restarts it whenever it
//! exits, which is how a new version starts after an update.

use std::path::{Path, PathBuf};
use std::process::Command;

use todesktop_updater::{Layout, Result, Updater};

const LABEL: &str = "com.acme.agent";

/// Settings worth keeping when launchd starts the agent, e.g. for tests.
const PASSED_THROUGH: [&str; 3] = [
    "ACME_CHECK_INTERVAL_SECS",
    "ACME_STATUS_FILE",
    "TODESKTOP_SMOKE_TEST_API_URL",
];

fn plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn domain() -> Result<String> {
    let output = Command::new("id").arg("-u").output()?;
    Ok(format!(
        "gui/{}",
        String::from_utf8_lossy(&output.stdout).trim()
    ))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist(program: &Path, log: &Path) -> String {
    let environment: String = PASSED_THROUGH
        .iter()
        .filter_map(|name| Some((name, std::env::var(name).ok()?)))
        .map(|(name, value)| {
            format!(
                "    <key>{name}</key>\n    <string>{}</string>\n",
                escape(&value)
            )
        })
        .collect();
    let log = escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
{environment}  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        escape(&program.to_string_lossy())
    )
}

/// Points launchd at the installed bundle's path, not at this build's
/// files: updates swap the bundle in place, so the path stays valid.
pub fn install(updater: &Updater) -> Result<()> {
    let Some(Layout::AppBundle { bundle }) = updater.install.as_ref().map(|i| &i.layout) else {
        return Err("--install-launch-agent needs an installed app bundle".into());
    };
    let program = bundle
        .join("Contents/MacOS")
        .join(&updater.identity.executable);
    let path = plist_path()?;
    let log = path
        .parent()
        .and_then(Path::parent)
        .ok_or("no Library folder")?
        .join("Logs/acme-agent.log");
    std::fs::create_dir_all(path.parent().ok_or("no LaunchAgents folder")?)?;
    std::fs::create_dir_all(log.parent().ok_or("no Logs folder")?)?;
    std::fs::write(&path, plist(&program, &log))?;

    let domain = domain()?;
    // Replace a previous registration, if any.
    Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .output()?;
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(&path)
        .status()?;
    if !status.success() {
        return Err(format!("launchctl bootstrap {domain} failed").into());
    }
    println!("acme-agent: launchd runs {} as {LABEL}", program.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let domain = domain()?;
    Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .output()?;
    let path = plist_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    println!("acme-agent: removed {LABEL}");
    Ok(())
}
