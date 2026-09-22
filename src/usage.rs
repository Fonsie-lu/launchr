//! Launch-count store used to rank the result list by popularity.
//!
//! Backed by a tab separated file so it stays trivially inspectable:
//! `count \t last_used_unix \t desktop_id`

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct Usage {
    entries: HashMap<String, (u32, u64)>,
    path: Option<PathBuf>,
}

impl Usage {
    pub fn load() -> Self {
        let Some(path) = store_path() else {
            return Self::default();
        };
        let mut entries = HashMap::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                let mut cols = line.splitn(3, '\t');
                let (Some(count), Some(last), Some(id)) = (cols.next(), cols.next(), cols.next())
                else {
                    continue;
                };
                let (Ok(count), Ok(last)) = (count.parse(), last.parse()) else {
                    continue;
                };
                if !id.is_empty() {
                    entries.insert(id.to_owned(), (count, last));
                }
            }
        }
        Self { entries, path: Some(path) }
    }

    pub fn get(&self, id: &str) -> (u32, u64) {
        self.entries.get(id).copied().unwrap_or((0, 0))
    }

    /// Count one launch of `id` and persist the store.
    pub fn record(&mut self, id: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let slot = self.entries.entry(id.to_owned()).or_insert((0, 0));
        slot.0 = slot.0.saturating_add(1);
        slot.1 = now;
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let Some(dir) = path.parent() else { return };
        if fs::create_dir_all(dir).is_err() {
            return;
        }

        let mut rows: Vec<_> = self.entries.iter().collect();
        rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(b.0)));
        let mut out = String::new();
        for (id, (count, last)) in rows {
            let _ = writeln!(out, "{count}\t{last}\t{id}");
        }

        // Write through a temp file so a crash cannot truncate the store. The
        // rename is atomic, but it says nothing about the bytes having reached
        // the disk, so flush them first — otherwise a power cut between write
        // and rename can leave an empty usage.tsv where the store used to be.
        // The directory entry the rename creates needs the same treatment, or
        // the rename itself is what a power cut loses.
        //
        // Both flushes cost real time on a busy disk. `activate` closes the
        // window before it gets here, so they delay the launcher's own exit
        // rather than the application it just started.
        let tmp = path.with_extension("tmp");
        let written = File::create(&tmp).and_then(|mut file| {
            file.write_all(out.as_bytes())?;
            file.sync_all()
        });
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        if fs::rename(&tmp, path).is_err() {
            return;
        }
        if let Ok(handle) = File::open(dir) {
            let _ = handle.sync_all();
        }
    }
}

fn store_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("launchr").join("usage.tsv"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (PathBuf, Usage) {
        let dir = std::env::temp_dir().join(format!("launchr-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("launchr").join("usage.tsv");
        (path.clone(), Usage { entries: HashMap::new(), path: Some(path) })
    }

    #[test]
    fn unknown_ids_report_zero() {
        let (_, usage) = temp_store("unknown");
        assert_eq!(usage.get("nope.desktop"), (0, 0));
    }

    #[test]
    fn record_counts_and_round_trips_through_disk() {
        let (path, mut usage) = temp_store("roundtrip");
        usage.record("firefox.desktop");
        usage.record("firefox.desktop");
        usage.record("foot.desktop");
        assert_eq!(usage.get("firefox.desktop").0, 2);

        let reloaded = Usage {
            entries: {
                let mut map = HashMap::new();
                for line in fs::read_to_string(&path).unwrap().lines() {
                    let cols: Vec<&str> = line.splitn(3, '\t').collect();
                    map.insert(cols[2].to_owned(), (cols[0].parse().unwrap(), cols[1].parse().unwrap()));
                }
                map
            },
            path: None,
        };
        assert_eq!(reloaded.get("firefox.desktop").0, 2);
        assert_eq!(reloaded.get("foot.desktop").0, 1);
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let (path, mut usage) = temp_store("corrupt");
        usage.record("good.desktop");
        fs::write(&path, "3\t100\tgood.desktop\ngarbage\nx\ty\tbad.desktop\n").unwrap();
        let loaded = {
            std::env::set_var("XDG_DATA_HOME", path.parent().unwrap().parent().unwrap());
            Usage::load()
        };
        assert_eq!(loaded.get("good.desktop").0, 3);
        assert_eq!(loaded.get("bad.desktop"), (0, 0));
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
