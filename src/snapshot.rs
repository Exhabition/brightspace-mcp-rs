use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CourseSnapshot {
    pub course: Value,
    pub synced_at: u64,
    pub tools: BTreeMap<String, Value>,
    pub errors: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SnapshotManifest {
    pub synced_at: Option<u64>,
    pub api_versions: Option<Value>,
    pub courses: Vec<Value>,
    pub tools: BTreeMap<String, Value>,
    pub errors: BTreeMap<String, String>,
}

pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn read_manifest(&self) -> Result<SnapshotManifest> {
        let path = self.root.join("manifest.json");
        if !path.exists() {
            return Ok(SnapshotManifest::default());
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn read_course(&self, course_id: u64) -> Result<CourseSnapshot> {
        let path = self.course_path(course_id);
        let bytes = fs::read(&path).with_context(|| {
            format!("course {course_id} has no local sync snapshot; run sync_courses first")
        })?;
        serde_json::from_slice(&bytes).context("course snapshot is invalid JSON")
    }

    pub fn write_manifest(&self, manifest: &SnapshotManifest) -> Result<()> {
        self.write_json(&self.root.join("manifest.json"), manifest)
    }

    pub fn write_course(&self, course_id: u64, snapshot: &CourseSnapshot) -> Result<()> {
        self.write_json(&self.course_path(course_id), snapshot)
    }

    pub fn clear(&self) -> Result<()> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root)
                .with_context(|| format!("remove snapshot directory {}", self.root.display()))?;
        }
        Ok(())
    }

    fn course_path(&self, course_id: u64) -> PathBuf {
        self.root.join("courses").join(format!("{course_id}.json"))
    }

    fn write_json(&self, path: &Path, value: &impl Serialize) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("snapshot path has no parent"))?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }

        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec_pretty(value)?;
        fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&temp, path).with_context(|| format!("commit snapshot {}", path.display()))?;
        Ok(())
    }
}

pub fn cache_key(name: &str, args: &BTreeMap<String, Value>) -> Result<String> {
    Ok(format!("{name}:{}", serde_json::to_string(args)?))
}

#[cfg(test)]
mod tests {
    use super::{CourseSnapshot, SnapshotManifest, SnapshotStore, cache_key};
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn snapshots_round_trip_and_missing_course_is_clear() {
        let root = std::env::temp_dir().join(format!(
            "brightspace-snapshot-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let store = SnapshotStore::new(root.clone());
        assert!(
            store
                .read_course(42)
                .unwrap_err()
                .to_string()
                .contains("sync_courses")
        );
        store
            .write_course(
                42,
                &CourseSnapshot {
                    course: json!({"Id": 42}),
                    synced_at: 7,
                    tools: BTreeMap::from([("get_syllabus:{}".to_owned(), json!({"Text":"hi"}))]),
                    errors: BTreeMap::new(),
                },
            )
            .expect("write snapshot");
        store
            .write_manifest(&SnapshotManifest {
                synced_at: Some(7),
                api_versions: None,
                courses: vec![json!({"Id": 42})],
                tools: BTreeMap::new(),
                errors: BTreeMap::new(),
            })
            .expect("write manifest");
        assert_eq!(store.read_course(42).expect("read snapshot").synced_at, 7);
        store.clear().expect("clear snapshot");
    }

    #[test]
    fn cache_key_is_stable_for_sorted_arguments() {
        let args = BTreeMap::from([("course_id".to_owned(), json!(42))]);
        assert_eq!(
            cache_key("get_syllabus", &args).expect("key"),
            "get_syllabus:{\"course_id\":42}"
        );
    }
}
