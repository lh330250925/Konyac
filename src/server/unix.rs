use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

pub(super) struct SocketGuard {
    path: PathBuf,
    metadata: Metadata,
    _lock: File,
}

impl SocketGuard {
    pub(super) async fn bind(path: &Path) -> io::Result<(UnixListener, Self)> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "Missing socket file name"))?;
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;

        let uid = rustix::process::geteuid().as_raw();
        let raw_dir = parent.symlink_metadata()?;
        if raw_dir.file_type().is_symlink() {
            if raw_dir.uid() != uid && raw_dir.uid() != 0 {
                return Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    format!("Unsafe Unix socket directory symlink: {}", parent.display()),
                ));
            }
        } else if !raw_dir.is_dir() {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("Unsafe Unix socket directory: {}", parent.display()),
            ));
        }

        let canonical_parent = parent.canonicalize()?;
        let dir = canonical_parent.symlink_metadata()?;
        let trusted_owner = dir.uid() == uid || dir.uid() == 0;
        let protected_entries = dir.mode() & 0o022 == 0 || dir.mode() & 0o1000 != 0;
        if !dir.is_dir() || !trusted_owner || !protected_entries {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("Unsafe Unix socket directory: {}", parent.display()),
            ));
        }
        let path = canonical_parent.join(name);
        if path.as_os_str().as_encoded_bytes().len() >= 104 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Unix socket path is too long (max 103 bytes): {}",
                    path.display()
                ),
            ));
        }
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(path.with_file_name(lock_name))?;
        let lock_metadata = lock.metadata()?;
        if !lock_metadata.is_file() || lock_metadata.uid() != uid {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "Unsafe Unix socket lock file",
            ));
        }
        // Keep the lock file in place so all instances contend on the same inode.
        lock.try_lock().map_err(|e| {
            io::Error::new(
                io::Error::from(e).kind(),
                format!("Cannot lock Unix socket path {}", path.display()),
            )
        })?;

        match path.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
                match tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(&path)).await
                {
                    Ok(Err(e)) if e.kind() == ErrorKind::ConnectionRefused => {
                        if !same_socket(&path, &metadata) {
                            return Err(io::Error::new(
                                ErrorKind::AddrInUse,
                                "Unix socket changed during startup",
                            ));
                        }
                        fs::remove_file(&path)?;
                    }
                    Ok(Err(e)) => return Err(e),
                    _ => {
                        return Err(io::Error::new(
                            ErrorKind::AddrInUse,
                            format!("Unix socket is in use: {}", path.display()),
                        ));
                    }
                }
            }
            Ok(_) => {
                return Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!("Refusing to replace existing path: {}", path.display()),
                ));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        let listener = UnixListener::bind(&path)?;
        let guard = Self {
            metadata: path.symlink_metadata()?,
            path,
            _lock: lock,
        };
        fs::set_permissions(&guard.path, fs::Permissions::from_mode(0o600))?;
        Ok((listener, guard))
    }
}

fn same_socket(path: &Path, expected: &Metadata) -> bool {
    path.symlink_metadata().is_ok_and(|actual| {
        actual.file_type().is_socket()
            && actual.dev() == expected.dev()
            && actual.ino() == expected.ino()
    })
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if same_socket(&self.path, &self.metadata) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socket_lifecycle_preserves_existing_paths() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("konyac-{unique:x}"));
        DirBuilder::new().mode(0o700).create(&directory).unwrap();
        let path = directory.join("test.sock");

        fs::write(&path, "keep this file").unwrap();
        assert!(SocketGuard::bind(&path).await.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep this file");
        fs::remove_file(&path).unwrap();

        let existing = UnixListener::bind(&path).unwrap();
        assert!(SocketGuard::bind(&path).await.is_err());
        drop(existing);

        let (listener, guard) = SocketGuard::bind(&path).await.unwrap();
        assert_eq!(path.symlink_metadata().unwrap().mode() & 0o777, 0o600);
        assert!(SocketGuard::bind(&path).await.is_err());
        assert!(UnixStream::connect(&path).await.is_ok());

        fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        drop(guard);
        assert!(UnixStream::connect(&path).await.is_ok());
        drop((listener, replacement));

        let (_listener, guard) = SocketGuard::bind(&path).await.unwrap();
        drop(guard);
        assert!(!path.exists());

        // Trusted symlink parent directory (e.g. /var/run -> /run) should be accepted.
        let link_dir = directory.join("link");
        std::os::unix::fs::symlink(&directory, &link_dir).unwrap();
        let linked_path = link_dir.join("linked.sock");
        let (linked_listener, linked_guard) = SocketGuard::bind(&linked_path).await.unwrap();
        assert!(UnixStream::connect(&linked_path).await.is_ok());
        drop((linked_listener, linked_guard));

        fs::remove_dir_all(directory).unwrap();
    }
}
