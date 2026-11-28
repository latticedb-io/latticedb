//! MANIFEST — the only mutable file (SPEC-03 §2).
//!
//! Content (TOML, ≤ 4 KiB, fixed field order): `{generation, snapshot_id,
//! parts, journal_segment, journal_offset, schema_epoch, created_lsn}`.
//!
//! Swap protocol: write `MANIFEST.tmp.<gen>` → fsync file → fsync directory
//! → atomic rename over `MANIFEST` → fsync directory. A kill-crash at any
//! point leaves either the old or the new MANIFEST fully valid (the rename
//! is atomic; the fsyncs guard power loss). Readers open MANIFEST before
//! every open/reopen and treat it as immutable (epoch marker).
//!
//! Garbage collection (SFT-2.2): stale `MANIFEST.tmp.*` files are removed at
//! open; parts absent from the MANIFEST and older than the reader-grace
//! window are deletable (window passed by the caller — a config parameter,
//! SPEC-02 §10).
//!
//! Segment retirement (SFT-6.3): segments below the checkpoint horizon move
//! to `journal/archive/` when `audit_retain = true` (present and
//! importable), or are deleted otherwise; active segments are untouched.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const MANIFEST_NAME: &str = "MANIFEST";
pub const ARCHIVE_SUBDIR: &str = "archive";
/// SPEC-03 §2: the MANIFEST is bounded at 4 KiB.
pub const MANIFEST_MAX_BYTES: usize = 4096;

#[derive(Debug)]
pub enum ManifestError {
    Io(io::Error),
    /// MANIFEST does not parse / violates the schema — names the problem.
    Invalid {
        reason: String,
    },
    /// Unknown or misspelled key — startup error naming it (strict law).
    UnknownKey {
        key: String,
    },
    TooLarge {
        len: usize,
    },
    Missing,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Io(e) => write!(f, "manifest i/o: {e}"),
            ManifestError::Invalid { reason } => write!(f, "manifest invalid: {reason}"),
            ManifestError::UnknownKey { key } => write!(f, "manifest: unknown key `{key}`"),
            ManifestError::TooLarge { len } => {
                write!(
                    f,
                    "manifest is {len} bytes; the cap is {MANIFEST_MAX_BYTES}"
                )
            }
            ManifestError::Missing => write!(f, "MANIFEST missing"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<io::Error> for ManifestError {
    fn from(e: io::Error) -> Self {
        ManifestError::Io(e)
    }
}

/// The persisted database pointer (SPEC-03 §2 content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub generation: u64,
    pub snapshot_id: u64,
    /// Live part generation numbers (part ids).
    pub parts: Vec<u64>,
    pub journal_segment: u32,
    pub journal_offset: u64,
    pub schema_epoch: u64,
    pub created_lsn: u64,
}

impl Manifest {
    pub fn fresh() -> Self {
        Self {
            generation: 1,
            snapshot_id: 0,
            parts: Vec::new(),
            journal_segment: 1,
            journal_offset: 0,
            schema_epoch: 0,
            created_lsn: 0,
        }
    }

    /// Deterministic TOML: fixed key order, ≤ 4 KiB (SPEC-03 §2).
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        let parts_csv: Vec<String> = self.parts.iter().map(|p| p.to_string()).collect();
        let s = format!(
            "generation = {}\nsnapshot_id = {}\nparts = [{}]\njournal_segment = {}\njournal_offset = {}\nschema_epoch = {}\ncreated_lsn = {}\n",
            self.generation,
            self.snapshot_id,
            parts_csv.join(", "),
            self.journal_segment,
            self.journal_offset,
            self.schema_epoch,
            self.created_lsn,
        );
        if s.len() > MANIFEST_MAX_BYTES {
            return Err(ManifestError::TooLarge { len: s.len() });
        }
        Ok(s)
    }

    /// Strict parse: unknown keys are errors naming the key (SPEC-02 §10
    /// strict-validation law applies to the MANIFEST like any config).
    pub fn from_toml(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MANIFEST_MAX_BYTES {
            return Err(ManifestError::TooLarge { len: text.len() });
        }
        let mut m = Self::fresh();
        let mut seen = [false; 7];
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| ManifestError::Invalid {
                reason: format!("line `{line}` is not `key = value`"),
            })?;
            let key = key.trim();
            let value = value.trim();
            match key {
                "generation" => {
                    m.generation = value.parse().map_err(|_| invalid_int(key, value))?;
                    seen[0] = true;
                }
                "snapshot_id" => {
                    m.snapshot_id = value.parse().map_err(|_| invalid_int(key, value))?;
                    seen[1] = true;
                }
                "parts" => {
                    let inner = value
                        .strip_prefix('[')
                        .and_then(|v| v.strip_suffix(']'))
                        .ok_or_else(|| ManifestError::Invalid {
                            reason: format!("parts must be `[a, b, ...]`, got `{value}`"),
                        })?;
                    let mut parts = Vec::new();
                    for item in inner.split(',') {
                        let item = item.trim();
                        if item.is_empty() {
                            continue;
                        }
                        parts.push(item.parse().map_err(|_| invalid_int("parts", item))?);
                    }
                    m.parts = parts;
                    seen[2] = true;
                }
                "journal_segment" => {
                    m.journal_segment = value.parse().map_err(|_| invalid_int(key, value))?;
                    seen[3] = true;
                }
                "journal_offset" => {
                    m.journal_offset = value.parse().map_err(|_| invalid_int(key, value))?;
                    seen[4] = true;
                }
                "schema_epoch" => {
                    m.schema_epoch = value.parse().map_err(|_| invalid_int(key, value))?;
                    seen[5] = true;
                }
                "created_lsn" => {
                    m.created_lsn = value.parse().map_err(|_| invalid_int(key, value))?;
                    seen[6] = true;
                }
                other => {
                    return Err(ManifestError::UnknownKey {
                        key: other.to_string(),
                    });
                }
            }
        }
        let names = [
            "generation",
            "snapshot_id",
            "parts",
            "journal_segment",
            "journal_offset",
            "schema_epoch",
            "created_lsn",
        ];
        for (i, present) in seen.iter().enumerate() {
            if !present {
                return Err(ManifestError::Invalid {
                    reason: format!("missing key `{}`", names[i]),
                });
            }
        }
        Ok(m)
    }
}

fn invalid_int(key: &str, value: &str) -> ManifestError {
    ManifestError::Invalid {
        reason: format!("key `{key}`: `{value}` is not an integer"),
    }
}

fn manifest_path(db_dir: &Path) -> PathBuf {
    db_dir.join(MANIFEST_NAME)
}

/// Read + parse the current MANIFEST.
pub fn read_manifest(db_dir: &Path) -> Result<Manifest, ManifestError> {
    let path = manifest_path(db_dir);
    if !path.exists() {
        return Err(ManifestError::Missing);
    }
    let mut text = String::new();
    File::open(&path)?.read_to_string(&mut text)?;
    Manifest::from_toml(&text)
}

/// Swap protocol (SPEC-03 §2): tmp → fsync file → fsync dir → atomic rename
/// → fsync dir. `crash_before_rename` simulates a kill between the tmp write
/// and the rename (SFT-2.1 loop driver).
pub fn swap_manifest(
    db_dir: &Path,
    next: &Manifest,
    crash_before_rename: bool,
) -> Result<(), ManifestError> {
    let tmp = db_dir.join(format!("MANIFEST.tmp.{}", next.generation));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(next.to_toml()?.as_bytes())?;
        f.sync_all()?; // fsync file
    }
    if crash_before_rename {
        return Ok(()); // crash: tmp stays, MANIFEST untouched
    }
    fs::rename(&tmp, manifest_path(db_dir))?;
    sync_dir(db_dir)?;
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<(), ManifestError> {
    let d = File::open(dir)?;
    d.sync_all()?;
    Ok(())
}

/// Open a database directory: read MANIFEST, then GC stale `MANIFEST.tmp.*`
/// files and orphan parts/journal segments that the MANIFEST does not list
/// and whose mtime age exceeds `grace_secs` (reader-grace window — config).
/// Returns the number of tmp files and orphans removed.
pub fn open_db_dir(
    db_dir: &Path,
    grace_secs: u64,
) -> Result<(Manifest, usize, usize), ManifestError> {
    let manifest = read_manifest(db_dir)?;

    // GC stale tmp files (always — they are never authoritative).
    let mut tmps_removed = 0usize;
    for entry in fs::read_dir(db_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("MANIFEST.tmp.") {
            fs::remove_file(entry.path())?;
            tmps_removed += 1;
        }
    }

    // GC orphan parts (listed parts are alive; unlisted + past grace go).
    let parts_dir = db_dir.join("parts");
    let mut orphans_removed = 0usize;
    if parts_dir.exists() {
        let listed: std::collections::BTreeSet<String> = manifest
            .parts
            .iter()
            .map(|p| format!("{:012}.lpart", p))
            .collect();
        for entry in fs::read_dir(&parts_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if listed.contains(&name) {
                continue;
            }
            let age = mtime_age_secs(&entry.path())?;
            if age > grace_secs {
                fs::remove_file(entry.path())?;
                orphans_removed += 1;
            }
        }
    }
    Ok((manifest, tmps_removed, orphans_removed))
}

fn mtime_age_secs(path: &Path) -> Result<u64, ManifestError> {
    let meta = fs::metadata(path)?;
    let modified = meta.modified().map_err(ManifestError::Io)?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .map_err(|_| ManifestError::Invalid {
            reason: "mtime in the future".into(),
        })?;
    Ok(age.as_secs())
}

const SNAPSHOT_PREFIX: &str = "snapshot.v";

/// Unlink `snapshot.v<N>` files other than `keep` whose mtime age exceeds
/// `grace_secs` — the reader-grace window of SPEC-03 §3 (the previous
/// snapshot is unlinked only once no reader can still hold it). Returns the
/// ids removed, ascending.
pub fn gc_snapshots(db_dir: &Path, keep: u64, grace_secs: u64) -> Result<Vec<u64>, ManifestError> {
    let mut removed = Vec::new();
    for entry in fs::read_dir(db_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_prefix(SNAPSHOT_PREFIX) else {
            continue;
        };
        if stem.is_empty() || !stem.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let id: u64 = stem.parse().map_err(|_| ManifestError::Invalid {
            reason: format!("snapshot id out of range: {name}"),
        })?;
        if id == keep {
            continue;
        }
        if mtime_age_secs(&entry.path())? > grace_secs {
            fs::remove_file(entry.path())?;
            removed.push(id);
        }
    }
    removed.sort_unstable();
    Ok(removed)
}

/// SFT-6.3: retire journal segments numbered `< below_segment`.
/// `audit_retain = true` → moved to `journal/archive/` (present and
/// importable); `false` → deleted. Active segments are untouched.
pub fn retire_segments(
    journal_dir: &Path,
    below_segment: u32,
    audit_retain: bool,
) -> Result<(usize, usize), ManifestError> {
    let archive_dir = journal_dir.join(ARCHIVE_SUBDIR);
    let mut archived = 0usize;
    let mut deleted = 0usize;
    for entry in fs::read_dir(journal_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            continue; // archive/ itself
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".log") else {
            continue;
        };
        let Ok(seg) = stem.parse::<u32>() else {
            continue;
        };
        if seg >= below_segment {
            continue; // active horizon
        }
        if audit_retain {
            fs::create_dir_all(&archive_dir)?;
            fs::rename(&path, archive_dir.join(name.as_ref()))?;
            archived += 1;
        } else {
            fs::remove_file(&path)?;
            deleted += 1;
        }
    }
    Ok((archived, deleted))
}

/// The archive subdirectory for retired journal segments (SFT-6.3).
pub fn archive_dir_of(journal_dir: &Path) -> PathBuf {
    journal_dir.join(ARCHIVE_SUBDIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip_is_exact() {
        let m = Manifest {
            generation: 7,
            snapshot_id: 3,
            parts: vec![12, 34, 56],
            journal_segment: 2,
            journal_offset: 999_999,
            schema_epoch: 41,
            created_lsn: 123_456,
        };
        let text = m.to_toml().expect("serialize");
        let back = Manifest::from_toml(&text).expect("parse");
        assert_eq!(back, m);
    }

    #[test]
    fn unknown_keys_and_missing_keys_rejected() {
        let good = Manifest::fresh().to_toml().unwrap();
        let typo = good.replace("journal_offset", "jurnal_offset");
        assert!(matches!(
            Manifest::from_toml(&typo),
            Err(ManifestError::UnknownKey { key }) if key == "jurnal_offset"
        ));
        let missing = good
            .lines()
            .filter(|l| !l.starts_with("schema_epoch"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matches!(
            Manifest::from_toml(&missing),
            Err(ManifestError::Invalid { reason }) if reason.contains("schema_epoch")
        ));
    }

    #[test]
    fn size_cap_enforced() {
        let mut m = Manifest::fresh();
        m.parts = vec![1u64; 100_000];
        assert!(matches!(m.to_toml(), Err(ManifestError::TooLarge { .. })));
    }
}
