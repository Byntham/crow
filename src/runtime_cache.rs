//! Remove obsolete cross-review package archives. New reviews install afresh.
use anyhow::{Result, ensure};
use std::{fs, path::Path};

fn archive_name(name: &str) -> bool {
    name.strip_suffix(".tar")
        .is_some_and(|key| key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub fn maintain(root: &Path) -> Result<u64> {
    crate::util::private_dir(root)?;
    ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "Runtime cache root is not a directory"
    );
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let lock = options.open(root.join("packages.lock"))?;
    ensure!(
        lock.metadata()?.is_file(),
        "Runtime cache lock is not a file"
    );
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut removed = 0;
    for (directory, packages) in [(root.to_owned(), false), (root.join("packages"), true)] {
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        ensure!(metadata.is_dir(), "Legacy cache path is not a directory");
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !archive_name(&name) && !(packages && name.starts_with(".crow-package-")) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_file() || metadata.is_symlink() {
                removed += metadata.len();
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_removes_all_legacy_archives_without_touching_images_or_evidence() {
        let root = tempfile::tempdir().unwrap();
        let packages = root.path().join("packages");
        fs::create_dir(&packages).unwrap();
        let name = format!("{}.tar", "a".repeat(64));
        for path in [
            root.path().join(&name),
            packages.join(&name),
            packages.join(".crow-package-partial"),
        ] {
            fs::write(path, b"obsolete").unwrap();
        }
        let image = root.path().join("managed-image.json");
        let evidence = packages.join("unrelated.txt");
        fs::write(&image, "image").unwrap();
        fs::write(&evidence, "evidence").unwrap();
        assert_eq!(maintain(root.path()).unwrap(), 24);
        assert_eq!(maintain(root.path()).unwrap(), 0);
        assert_eq!(fs::read_to_string(image).unwrap(), "image");
        assert_eq!(fs::read_to_string(evidence).unwrap(), "evidence");
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_does_not_follow_legacy_archive_or_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let name = format!("{}.tar", "b".repeat(64));
        let target = external.path().join(&name);
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join(&name)).unwrap();
        maintain(root.path()).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        std::os::unix::fs::symlink(external.path(), root.path().join("packages")).unwrap();
        assert!(maintain(root.path()).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
    }
}
