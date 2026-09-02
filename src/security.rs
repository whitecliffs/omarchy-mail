use std::fs;
use std::io;
use std::path::Path;

/// Keep mail cache, drafts, and preferences private on the Unix systems this
/// application targets. On other platforms the filesystem's normal defaults
/// remain in effect.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_private_dir_permissions(path)
}

pub fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn set_private_dir_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn private_paths_use_owner_only_permissions() {
        let directory = tempdir().expect("temporary directory");
        let private_dir = directory.path().join("mail");
        ensure_private_dir(&private_dir).expect("private directory");
        assert_eq!(
            fs::metadata(&private_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let private_file = private_dir.join("preferences.json");
        fs::write(&private_file, b"{}").expect("private file");
        set_private_file_permissions(&private_file).expect("private file permissions");
        assert_eq!(
            fs::metadata(&private_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
