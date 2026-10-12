//! Replacing a macOS app bundle with the one from an update zip.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::identity::{is_app_bundle, BuildIdentity};
use crate::record::BuildRecord;
use crate::Result;

/// How the staged bundle took the installed one's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swap {
    /// One atomic `renamex_np(RENAME_SWAP)`.
    Atomic,
    /// The installed bundle was renamed aside, then the staged one into
    /// place: briefly, nothing is installed.
    RenameAside,
}

impl Swap {
    pub fn as_str(self) -> &'static str {
        match self {
            Swap::Atomic => "renamex_np",
            Swap::RenameAside => "rename-aside",
        }
    }
}

/// Unzips next to the installed bundle (same volume, so the swap is a
/// rename), checks the staged bundle is the signed record's build and is
/// signed like the installed one, then swaps it in and deletes the old
/// bundle. The caller exits so launchd starts the new version.
pub fn apply_app_bundle(installed: &Path, archive: &Path, record: &BuildRecord) -> Result<Swap> {
    let parent = installed
        .parent()
        .ok_or("installed app has no parent folder")?;
    let name = installed
        .file_name()
        .ok_or("installed app has no name")?
        .to_string_lossy();
    let staging = parent.join(format!(".{name}.{}.staging", record.build_id));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir(&staging).map_err(|err| -> crate::Error {
        if err.kind() == io::ErrorKind::PermissionDenied {
            format!(
                "cannot update {}: {} is not writable by this user (installing there needs a privileged helper)",
                installed.display(),
                parent.display()
            )
            .into()
        } else {
            err.into()
        }
    })?;

    let result = stage_and_swap(installed, archive, record, &staging);
    // After a swap the staging folder holds the old bundle.
    fs::remove_dir_all(&staging).ok();
    result
}

fn stage_and_swap(
    installed: &Path,
    archive: &Path,
    record: &BuildRecord,
    staging: &Path,
) -> Result<Swap> {
    run(Command::new("ditto")
        .arg("-x")
        .arg("-k")
        .arg(archive)
        .arg(staging))?;
    let staged = single_app(staging)?;
    let identity = BuildIdentity::load(&staged)?;
    if identity.app_id != record.app_id || identity.build_id != record.build_id {
        return Err("staged app's build identity does not match the signed record".into());
    }
    verify_signature(&staged, installed)?;
    swap(&staged, installed)
}

fn single_app(dir: &Path) -> Result<PathBuf> {
    let mut apps = fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_app_bundle(path));
    match (apps.next(), apps.next()) {
        (Some(app), None) => Ok(app),
        _ => Err("update zip must contain exactly one .app".into()),
    }
}

/// The staged bundle must be validly signed and sealed. When the
/// installed app is signed by a team (Developer ID), the staged one must
/// satisfy the installed one's designated requirement: same team, same
/// bundle ID. Ad-hoc signed apps have no team and a requirement that
/// names their exact code hash, so for them only validity is checked.
fn verify_signature(staged: &Path, installed: &Path) -> Result<()> {
    run(Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(staged))
    .map_err(|err| format!("staged app's signature is not valid: {err}"))?;

    let Some(team) = team_id(installed)? else {
        return Ok(());
    };
    let staged_team = team_id(staged)?;
    if staged_team.as_deref() != Some(team.as_str()) {
        return Err(format!(
            "staged app is signed by team {}, the installed app by {team}",
            staged_team.as_deref().unwrap_or("none (ad-hoc)")
        )
        .into());
    }
    let requirement = designated_requirement(installed)?;
    run(Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(format!("-R={requirement}"))
        .arg(staged))
    .map_err(|err| format!("staged app does not satisfy the installed app's requirement: {err}"))?;
    Ok(())
}

/// `TeamIdentifier` from `codesign -dvv`, or None when ad-hoc signed.
pub fn team_id(app: &Path) -> Result<Option<String>> {
    // codesign -d writes its report to stderr.
    let report = run(Command::new("codesign").arg("-dvv").arg(app))?.stderr;
    Ok(report
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .filter(|team| *team != "not set")
        .map(str::to_owned))
}

fn designated_requirement(app: &Path) -> Result<String> {
    let output = run(Command::new("codesign").args(["-d", "-r-"]).arg(app))?;
    output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .find_map(|line| line.strip_prefix("designated => "))
        .map(str::to_owned)
        .ok_or_else(|| "installed app has no designated requirement".into())
}

struct Output {
    stdout: String,
    stderr: String,
}

fn run(command: &mut Command) -> Result<Output> {
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        let program = command.get_program().to_string_lossy();
        return Err(format!("{program} failed: {}", stderr.trim()).into());
    }
    Ok(Output { stdout, stderr })
}

/// Exchanges the two bundles in one rename (APFS and HFS+ support it),
/// so the app is never missing. Falls back to renaming the installed
/// bundle aside when the volume can't swap.
fn swap(staged: &Path, installed: &Path) -> Result<Swap> {
    match rename_swap(staged, installed) {
        Ok(()) => Ok(Swap::Atomic),
        Err(err) => {
            eprintln!("todesktop-updater: RENAME_SWAP failed ({err}); renaming aside");
            let aside = staged.with_extension("app.old");
            fs::rename(installed, &aside)?;
            if let Err(err) = fs::rename(staged, installed) {
                fs::rename(&aside, installed).ok();
                return Err(err.into());
            }
            Ok(Swap::RenameAside)
        }
    }
}

#[cfg(target_os = "macos")]
fn rename_swap(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both are valid NUL-terminated paths for the whole call.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn rename_swap(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "RENAME_SWAP is macOS only",
    ))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::identity::IDENTITY_FILE;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join(format!("td-app-bundle-{}", uuid::Uuid::new_v4().simple()));
            fs::create_dir_all(&dir).unwrap();
            // Bundle paths are compared after canonicalization (/private/var).
            Self(dir.canonicalize().unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let mut permissions = fs::metadata(&self.0).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&self.0, permissions).ok();
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// An ad-hoc signed bundle with the given build, like the runner makes.
    fn make_app(dir: &Path, build_id: &str, build_number: u64) -> PathBuf {
        let app = dir.join("Acme Agent.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        fs::copy("/usr/bin/true", app.join("Contents/MacOS/acme-agent")).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>acme-agent</string>
<key>CFBundleIdentifier</key><string>com.acme.agent</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#,
        )
        .unwrap();
        fs::write(
            app.join("Contents/Resources").join(IDENTITY_FILE),
            format!(
                r#"{{"appId":"acme-agent","buildId":"{build_id}","buildNumber":{build_number},"executable":"acme-agent","installKind":"zip","updateBaseUrl":"http://x","updateKeys":[],"version":"{build_number}.0.0"}}"#
            ),
        )
        .unwrap();
        run(Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app))
        .unwrap();
        app
    }

    fn zip(app: &Path, out: &Path) {
        run(Command::new("ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(app)
            .arg(out))
        .unwrap();
    }

    fn record(build_id: &str, build_number: u64) -> BuildRecord {
        serde_json::from_value(serde_json::json!({
            "appId": "acme-agent",
            "buildId": build_id,
            "buildNumber": build_number,
            "files": [],
            "schema": 1,
            "version": format!("{build_number}.0.0"),
        }))
        .unwrap()
    }

    /// An installed 1.0.0 in `<tmp>/Applications` and a 2.0.0 update zip.
    fn setup() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new();
        let installed = make_app(&tmp.0.join("Applications"), "b1", 1);
        let update = make_app(&tmp.0.join("build"), "b2", 2);
        let archive = tmp.0.join("update.zip");
        zip(&update, &archive);
        (tmp, installed, archive)
    }

    fn installed_build(app: &Path) -> String {
        BuildIdentity::load(app).unwrap().build_id
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect()
    }

    #[test]
    fn swaps_the_bundle_atomically_and_cleans_up() {
        let (_tmp, installed, archive) = setup();
        let swap = apply_app_bundle(&installed, &archive, &record("b2", 2)).unwrap();
        assert_eq!(swap, Swap::Atomic);
        assert_eq!(installed_build(&installed), "b2");
        run(Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&installed))
        .unwrap();
        assert!(leftovers(installed.parent().unwrap()).is_empty());
    }

    #[test]
    fn refuses_a_zip_whose_identity_is_not_the_record() {
        let (_tmp, installed, archive) = setup();
        let err = apply_app_bundle(&installed, &archive, &record("b3", 3)).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        assert_eq!(installed_build(&installed), "b1");
        assert!(leftovers(installed.parent().unwrap()).is_empty());
    }

    #[test]
    fn refuses_a_bundle_modified_after_signing() {
        let tmp = TempDir::new();
        let installed = make_app(&tmp.0.join("Applications"), "b1", 1);
        let update = make_app(&tmp.0.join("build"), "b2", 2);
        fs::write(update.join("Contents/Resources/extra.txt"), "tampered").unwrap();
        let archive = tmp.0.join("update.zip");
        zip(&update, &archive);
        let err = apply_app_bundle(&installed, &archive, &record("b2", 2)).unwrap_err();
        assert!(err.to_string().contains("signature is not valid"), "{err}");
        assert_eq!(installed_build(&installed), "b1");
    }

    #[test]
    fn explains_when_the_folder_is_not_writable() {
        let (_tmp, installed, archive) = setup();
        let applications = installed.parent().unwrap();
        fs::set_permissions(applications, fs::Permissions::from_mode(0o555)).unwrap();
        let err = apply_app_bundle(&installed, &archive, &record("b2", 2)).unwrap_err();
        fs::set_permissions(applications, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.to_string().contains("not writable"), "{err}");
        assert_eq!(installed_build(&installed), "b1");
    }

    #[test]
    fn ad_hoc_apps_have_no_team() {
        let tmp = TempDir::new();
        let app = make_app(&tmp.0, "b1", 1);
        assert_eq!(team_id(&app).unwrap(), None);
    }
}
