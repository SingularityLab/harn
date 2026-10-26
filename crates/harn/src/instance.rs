use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::{Context, Result, bail};

const WATCH_LOCK_FILE: &str = "watch.lock";

/// Two watchers would both type `continue` into the same terminal. The OS drops the lock when the
/// process dies, so a crash never leaves a stale lock behind. Keep the returned file alive.
pub fn single_instance(cache_dir: &Path) -> Result<File> {
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("cannot create {}", cache_dir.display()))?;
    let path = cache_dir.join(WATCH_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!("harn is already watching (another `harn watch` or harn-tray is running)")
        }
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_watcher_is_refused_until_the_first_exits() {
        let dir = std::env::temp_dir().join(format!("harn-watch-lock-{}", std::process::id()));
        let first = single_instance(&dir).unwrap();
        let err = single_instance(&dir).unwrap_err();
        assert!(err.to_string().contains("already watching"), "{err:#}");
        drop(first);
        assert!(single_instance(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
