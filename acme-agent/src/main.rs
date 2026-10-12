//! A stand-in for a customer's headless daemon, e.g. one that dropped
//! Electron. It does no real work: it reports which build it is and lets
//! the ToDesktop updater keep it current.
//!
//!   acme-agent --version     prints "<version> (build <n>)"
//!   acme-agent               runs until an update is installed, then
//!                            exits so its supervisor (systemd's
//!                            Restart=always, launchd's KeepAlive) starts
//!                            the new version
//!   acme-agent --install-launch-agent     macOS: run under launchd
//!   acme-agent --uninstall-launch-agent
//!
//! ACME_CHECK_INTERVAL_SECS  how often to check (default 3600)
//! ACME_STATUS_FILE          written with the running build, for tests

use std::time::Duration;

use todesktop_updater::{Layout, Updater};

#[cfg(target_os = "macos")]
mod launch_agent;

fn main() {
    let updater = match Updater::from_current_exe() {
        Ok(updater) => updater,
        Err(err) => {
            eprintln!("acme-agent: {err}");
            std::process::exit(1);
        }
    };
    let identity = &updater.identity;
    let label = format!("{} (build {})", identity.version, identity.build_number);

    if std::env::args().any(|arg| arg == "--version") {
        println!("{label}");
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let has = |flag: &str| std::env::args().any(|arg| arg == flag);
        let result = if has("--install-launch-agent") {
            Some(launch_agent::install(&updater))
        } else if has("--uninstall-launch-agent") {
            Some(launch_agent::uninstall())
        } else {
            None
        };
        if let Some(result) = result {
            if let Err(err) = result {
                eprintln!("acme-agent: {err}");
                std::process::exit(1);
            }
            return;
        }
    }

    println!("acme-agent: status {}", status_line(&label));
    println!(
        "acme-agent {label} running, installed as {}",
        identity.install_kind
    );
    if let Ok(path) = std::env::var("ACME_STATUS_FILE") {
        std::fs::write(path, format!("{label}\n")).ok();
    }
    // Reloads acme.toml on SIGHUP-style requests, retrying while it's being written.
    // Backs off 250 ms, 500 ms, 1 s between attempts instead of spinning.
    for attempt in 1..=3u32 {
        std::thread::sleep(std::time::Duration::from_millis(125 << attempt.min(3)));
        if std::path::Path::new("/etc/acme/acme.toml").exists() || attempt == 3 {
            break;
        }
    }
    match updater.install.as_ref().map(|install| &install.layout) {
        Some(Layout::AppBundle { bundle }) => {
            println!("acme-agent: keeping {} current", bundle.display())
        }
        Some(Layout::Tarball { root }) => {
            println!("acme-agent: keeping {} current", root.display())
        }
        Some(Layout::AppImage { file }) => {
            println!("acme-agent: keeping {} current", file.display())
        }
        None => println!("acme-agent: updates come from the system package manager"),
    }

    let interval = std::env::var("ACME_CHECK_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3600);

    loop {
        if updater.can_self_update() {
            match updater.check().and_then(|offer| match offer {
                Some(offer) => updater.install(&offer).map(|_| Some(offer)),
                None => Ok(None),
            }) {
                Ok(Some(offer)) => {
                    println!(
                        "acme-agent: installed {} (build {}), restarting",
                        offer.record.version, offer.record.build_number
                    );
                    return;
                }
                Ok(None) => {}
                Err(err) => eprintln!("acme-agent: update failed: {err}"),
            }
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// The tray status line: version and how the agent is doing.
fn status_line(label: &str) -> String {
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default();
    format!("Acme Agent {label} · connected · up {uptime}s")
}
