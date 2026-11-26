use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::Context as _;
use crate::Result;

/// Where a disruptor keeps what it needs to revert: one JSON file per
/// instance, `<state dir>/<plugin>/<instance>.json`, the state dir being
/// [`ENV_STATE_DIR`](crate::protocol::ENV_STATE_DIR).
///
/// A file is written atomically (a temporary file, synced, then renamed),
/// so a crash leaves either the previous state or the new one.
#[derive(Debug, Clone)]
pub struct StateStore {
    dir: Option<PathBuf>,
    plugin: String,
}

impl StateStore {
    /// The store of `plugin` under `state_dir`; `None` when the agent gave
    /// no state directory (every access then fails).
    pub fn new(state_dir: Option<PathBuf>, plugin: impl Into<String>) -> Self {
        Self {
            dir: state_dir,
            plugin: plugin.into(),
        }
    }

    /// The store of `plugin` under the directory the agent set in the
    /// environment.
    pub fn from_env(plugin: impl Into<String>) -> Self {
        Self::new(state_dir_from_env(), plugin)
    }

    /// The file holding `instance`'s state.
    pub fn path(&self, instance: &str) -> Result<PathBuf> {
        let Some(dir) = &self.dir else {
            crate::bail!(
                "{} is not set: no state directory",
                crate::protocol::ENV_STATE_DIR
            );
        };
        check_name("plugin", &self.plugin)?;
        check_name("instance", instance)?;
        Ok(dir.join(&self.plugin).join(format!("{instance}.json")))
    }

    /// Saves `instance`'s state, replacing the previous one.
    pub fn save<T: Serialize + ?Sized>(&self, instance: &str, state: &T) -> Result {
        let path = self.path(instance)?;
        let json = serde_json::to_vec_pretty(state).context("serializing the state")?;
        write_atomic(&path, &json).with_context(|| format!("saving {}", path.display()))
    }

    /// Loads `instance`'s state; `None` when none was saved.
    pub fn load<T: DeserializeOwned>(&self, instance: &str) -> Result<Option<T>> {
        let path = self.path(instance)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(crate::Error::from(e).context(format!("reading {}", path.display())))
            }
        };
        let state = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(state))
    }

    /// Removes `instance`'s state; nothing to do when there is none.
    pub fn clear(&self, instance: &str) -> Result {
        let path = self.path(instance)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(crate::Error::from(e).context(format!("removing {}", path.display()))),
        }
    }
}

pub(crate) fn state_dir_from_env() -> Option<PathBuf> {
    std::env::var_os(crate::protocol::ENV_STATE_DIR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// A name used as a path component: no separator, not hidden, not empty.
fn check_name(what: &str, name: &str) -> Result {
    let bad = name.is_empty()
        || name.starts_with('.')
        || name.contains(['/', '\\', '\0'])
        || name.len() > 200;
    if bad {
        crate::bail!("invalid {what} name {name:?} for a state file");
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = dir.join(tmp_name);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    // Make the rename itself durable; not every platform can open a
    // directory, so this is best effort.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_clear() {
        let dir = crate::test_dir("state");
        let store = StateStore::new(Some(dir.clone()), "marker");
        assert_eq!(store.load::<u32>("d1").unwrap(), None);
        store.save("d1", &42u32).unwrap();
        assert!(dir.join("marker/d1.json").is_file());
        assert!(!dir.join("marker/d1.json.tmp").exists());
        assert_eq!(store.load::<u32>("d1").unwrap(), Some(42));
        store.clear("d1").unwrap();
        store.clear("d1").unwrap();
        assert_eq!(store.load::<u32>("d1").unwrap(), None);
    }

    #[test]
    fn rejects_bad_names_and_missing_dir() {
        let store = StateStore::new(Some(crate::test_dir("names")), "p");
        for bad in ["", "../x", "a/b", ".hidden"] {
            assert!(store.path(bad).is_err(), "{bad:?}");
        }
        let err = StateStore::new(None, "p").save("d1", &1).unwrap_err();
        assert!(err.message().contains("REMORA_QA_STATE_DIR"), "{err}");
    }
}
