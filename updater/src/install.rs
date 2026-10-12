use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::identity::BuildIdentity;
use crate::record::{hex, BuildRecord};
use crate::Result;

/// `<root>/versions/<buildId>/` → `<root>`, or None for other layouts.
pub fn tarball_root(app_dir: &Path) -> Option<PathBuf> {
    let versions = app_dir.parent()?;
    if versions.file_name()? != "versions" {
        return None;
    }
    versions.parent().map(Path::to_path_buf)
}

/// Counts bytes as they're read, for download progress.
pub struct Progress<R, F> {
    inner: R,
    on_read: F,
    read: u64,
}

impl<R, F: FnMut(u64)> Progress<R, F> {
    pub fn new(inner: R, on_read: F) -> Self {
        Self {
            inner,
            on_read,
            read: 0,
        }
    }
}

impl<R: Read, F: FnMut(u64)> Read for Progress<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.read += read as u64;
        (self.on_read)(self.read);
        Ok(read)
    }
}

pub struct Downloaded {
    pub sha256: String,
    pub size: u64,
}

/// Streams to `<path>.partial`, hashing as it goes, then renames.
pub fn download_to(reader: &mut impl Read, path: &Path) -> Result<Downloaded> {
    let partial = path.with_extension("partial");
    let mut file = fs::File::create(&partial)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
        size += read as u64;
    }
    file.sync_all()?;
    fs::rename(&partial, path)?;
    Ok(Downloaded {
        sha256: hex(&hasher.finalize()),
        size,
    })
}

/// Replaces an AppImage with the downloaded one in one rename: copied
/// beside it first (same filesystem), made executable and synced, so the
/// file is always either the old build or the new one. The running
/// process keeps its own mount until it restarts.
pub fn apply_appimage(file: &Path, archive: &Path) -> Result<PathBuf> {
    let dir = file.parent().ok_or("the AppImage has no parent folder")?;
    let name = file
        .file_name()
        .ok_or("the AppImage has no name")?
        .to_string_lossy();
    let staging = dir.join(format!(".{name}.partial"));
    fs::copy(archive, &staging)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
    }
    fs::File::open(&staging)?.sync_all()?;
    if let Err(err) = fs::rename(&staging, file) {
        fs::remove_file(&staging).ok();
        return Err(err.into());
    }
    Ok(file.to_path_buf())
}

/// Unpacks next to the running version, checks the identity inside
/// matches the signed record, then swaps the `current` link in one
/// rename. The old version stays until the next update, for rollback.
pub fn apply_tarball(root: &Path, archive: &Path, record: &BuildRecord) -> Result<PathBuf> {
    let versions = root.join("versions");
    let target = versions.join(&record.build_id);
    let staging = versions.join(format!(".{}.partial", record.build_id));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }

    let mut tar = tar::Archive::new(GzDecoder::new(fs::File::open(archive)?));
    tar.set_preserve_permissions(true);
    // tar::Archive::unpack refuses entries that escape the directory.
    tar.unpack(&staging)?;

    let identity = BuildIdentity::load(&staging)?;
    if identity.app_id != record.app_id || identity.build_id != record.build_id {
        fs::remove_dir_all(&staging)?;
        return Err("archive's build identity does not match the signed record".into());
    }

    if target.exists() {
        fs::remove_dir_all(&target)?;
    }
    fs::rename(&staging, &target)?;

    let current = root.join("current");
    let previous = fs::read_link(&current).ok();
    let next_link = root.join(".current.next");
    if next_link.symlink_metadata().is_ok() {
        fs::remove_file(&next_link)?;
    }
    std::os::unix::fs::symlink(Path::new("versions").join(&record.build_id), &next_link)?;
    fs::rename(&next_link, &current)?;

    prune(&versions, &record.build_id, previous.as_deref());
    Ok(target)
}

/// Keeps the new version and the one it replaced; removes the rest.
fn prune(versions: &Path, keep: &str, previous: Option<&Path>) {
    let previous = previous.and_then(Path::file_name);
    let Ok(entries) = fs::read_dir(versions) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == keep
            || Some(name.as_os_str()) == previous
            || name.to_string_lossy().starts_with('.')
        {
            continue;
        }
        fs::remove_dir_all(entry.path()).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn replaces_an_appimage_whole_and_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("td-appimage-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("acme-agent.AppImage");
        let archive = dir.join("download");
        fs::write(&file, b"build 1").unwrap();
        fs::write(&archive, b"build 2").unwrap();

        assert_eq!(apply_appimage(&file, &archive).unwrap(), file);
        assert_eq!(fs::read(&file).unwrap(), b"build 2");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["acme-agent.AppImage", "download"]);
        fs::remove_dir_all(dir).unwrap();
    }
}
