//! Newest-snapshot mirror file.
//!
//! The Noctalia dock cannot yet consume the `subscribe` stream, so it polls.
//! Polling a file it can read directly costs a stat and a read; polling by
//! forking `niri-xmb snapshot` costs a process, a socket and a full
//! serialisation. This exists so the widget can do the former, and it deletes
//! itself the day the stream lands.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Where the daemon publishes the newest snapshot for pollers.
pub fn mirror_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(dir).join("niri-xmb.snapshot.json"))
}

/// A snapshot file rewritten atomically on every generation.
pub struct SnapshotMirror {
    path: PathBuf,
    temp: PathBuf,
}

impl SnapshotMirror {
    /// `None` when there is no runtime directory to write into, which is not
    /// an error: the daemon simply runs without a mirror and pollers fall back
    /// to the control socket.
    pub fn new() -> Option<Self> {
        let path = mirror_path()?;
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        Some(Self { path, temp })
    }

    /// Replace the mirror with `body`.
    ///
    /// Written to a temporary name in the same directory and renamed, so a
    /// reader either sees the previous complete snapshot or the new one, never
    /// a half-written file. Mode 0600: a snapshot carries window titles.
    pub fn write(&self, body: &str) {
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.temp)?;
            file.write_all(body.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_data()?;
            drop(file);
            std::fs::rename(&self.temp, &self.path)
        };
        if let Err(error) = write() {
            tracing::debug!(%error, path = %self.path.display(), "failed to mirror snapshot");
        }
    }
}

impl Drop for SnapshotMirror {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp);
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_replaces_its_file_atomically_and_stays_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("niri-xmb-mirror-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mirror = SnapshotMirror {
            path: dir.join("snapshot.json"),
            temp: dir.join("snapshot.json.tmp"),
        };

        mirror.write("{\"generation\":1}");
        assert_eq!(
            std::fs::read_to_string(&mirror.path).unwrap().trim(),
            "{\"generation\":1}"
        );

        mirror.write("{\"generation\":2}");
        assert_eq!(
            std::fs::read_to_string(&mirror.path).unwrap().trim(),
            "{\"generation\":2}"
        );

        let mode = std::fs::metadata(&mirror.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "a snapshot carries window titles");
        // The temporary name must not be left behind.
        assert!(!mirror.temp.exists());

        drop(mirror);
        std::fs::remove_dir_all(&dir).ok();
    }
}
