//! Sidecar for apps that don't link the library:
//!
//!   todesktop-updater check  [--app-dir DIR]   prints the offer, if any
//!   todesktop-updater update [--app-dir DIR]   installs it
//!
//! Exit codes: 0 up to date, 10 updated (restart the app), 1 error.

use std::path::PathBuf;
use std::process::ExitCode;

use todesktop_updater::Updater;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("todesktop-updater: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> todesktop_updater::Result<ExitCode> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let mut app_dir: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        if arg == "--app-dir" {
            app_dir = args.next().map(PathBuf::from);
        }
    }
    let updater = match app_dir {
        Some(dir) => Updater::from_app_dir(&dir)?,
        None => Updater::from_current_exe()?,
    };
    if !updater.can_self_update() {
        println!(
            "{}",
            serde_json::json!({ "status": "managed", "installKind": updater.identity.install_kind })
        );
        return Ok(ExitCode::SUCCESS);
    }

    let offer = updater.check()?;
    match (command.as_str(), offer) {
        (_, None) => {
            println!("{}", serde_json::json!({ "status": "up-to-date" }));
            Ok(ExitCode::SUCCESS)
        }
        ("check", Some(offer)) => {
            println!(
                "{}",
                serde_json::json!({
                    "status": "available",
                    "version": offer.record.version,
                    "buildNumber": offer.record.build_number,
                    "rolloutId": offer.rollout_id,
                })
            );
            Ok(ExitCode::SUCCESS)
        }
        ("update", Some(offer)) => {
            let dir = updater.install(&offer)?;
            println!(
                "{}",
                serde_json::json!({
                    "status": "updated",
                    "version": offer.record.version,
                    "buildNumber": offer.record.build_number,
                    "dir": dir,
                })
            );
            Ok(ExitCode::from(10))
        }
        _ => Err("usage: todesktop-updater check|update [--app-dir DIR]".into()),
    }
}
