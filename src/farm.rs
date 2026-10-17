use crate::config::{data_dir, write_atomic};
use crate::gcode;
use crate::printer::{Job, Outcome};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now() -> u64 {
    unix(SystemTime::now())
}

pub fn queue_path() -> PathBuf {
    data_dir().join("queue.json")
}

pub fn history_path() -> PathBuf {
    data_dir().join("history.jsonl")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Running {
    pub job: Job,
    pub started: u64,
    #[serde(default)]
    pub on_printer: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Queue {
    #[serde(default)]
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub unclear: BTreeSet<String>,
    #[serde(default)]
    pub running: BTreeMap<String, Running>,
}

impl Queue {
    pub fn load(path: &Path) -> Result<Queue, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Queue::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_atomic(path, text.as_bytes())
    }

    pub fn add(&mut self, path: &Path, printer: Option<String>) -> Result<u64, String> {
        let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !gcode::is_gcode(&path) {
            return Err(format!("{} is not a G-code file", path.display()));
        }
        let meta = gcode::scan(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.commands == 0 {
            return Err(format!("{} has no G-code in it", path.display()));
        }
        self.next_id = self.next_id.max(self.jobs.iter().map(|j| j.id).max().unwrap_or(0)) + 1;
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.jobs.push(Job {
            id: self.next_id,
            name,
            path,
            printer,
            added: now(),
            meta,
            card: None,
        });
        Ok(self.next_id)
    }

    pub fn find(&self, id: u64) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    pub fn take(&mut self, id: u64) -> Option<Job> {
        let i = self.jobs.iter().position(|j| j.id == id)?;
        Some(self.jobs.remove(i))
    }

    pub fn shift(&mut self, id: u64, by: isize) {
        let Some(i) = self.jobs.iter().position(|j| j.id == id) else { return };
        let to = (i as isize + by).clamp(0, self.jobs.len() as isize - 1) as usize;
        let job = self.jobs.remove(i);
        self.jobs.insert(to, job);
    }

    pub fn assign(&mut self, id: u64, printer: Option<String>) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
            j.printer = printer;
        }
    }

    pub fn next_for(&self, printer: &str, loaded: Option<&str>) -> Option<&Job> {
        self.jobs.iter().find(|j| for_printer(j, printer) && fits(j, loaded))
    }

    pub fn waiting_for_filament(&self, printer: &str, loaded: Option<&str>) -> Vec<String> {
        let mut out: Vec<String> = self
            .jobs
            .iter()
            .filter(|j| for_printer(j, printer) && !fits(j, loaded))
            .filter_map(|j| j.meta.filament.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn fill_in(&mut self) -> bool {
        let mut changed = false;
        for j in self.jobs.iter_mut().filter(|j| j.meta.filament.is_none()) {
            if let Ok(meta) = gcode::scan(&j.path)
                && meta.filament.is_some()
            {
                j.meta.filament = meta.filament;
                changed = true;
            }
        }
        changed
    }
}

fn for_printer(job: &Job, printer: &str) -> bool {
    job.printer.as_deref().is_none_or(|p| p == printer)
}

pub fn fits(job: &Job, loaded: Option<&str>) -> bool {
    match (job.meta.filament.as_deref(), loaded) {
        (Some(want), Some(have)) => want.eq_ignore_ascii_case(have),
        _ => true,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub job: String,
    pub path: PathBuf,
    pub printer: String,
    pub started: u64,
    pub ended: u64,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filament_g: Option<f32>,
}

impl Record {
    pub fn new(job: &Job, printer: &str, started: u64, outcome: Outcome, reason: &str) -> Record {
        Record {
            job: job.name.clone(),
            path: job.path.clone(),
            printer: printer.to_string(),
            started,
            ended: now(),
            outcome,
            reason: reason.to_string(),
            filament_g: job.meta.filament_g.filter(|_| outcome == Outcome::Done),
        }
    }

    pub fn seconds(&self) -> u64 {
        self.ended.saturating_sub(self.started)
    }
}

pub fn append(path: &Path, record: &Record) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut line = serde_json::to_string(record).map_err(|e| e.to_string())?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn history(path: &Path) -> Vec<Record> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("printfarm-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn queue_orders_and_routes_jobs() {
        let dir = scratch("queue");
        for n in ["a", "b", "c"] {
            std::fs::write(dir.join(format!("{n}.gcode")), "G28\nG1 X1\n").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "G28\n").unwrap();
        let mut q = Queue::default();
        let a = q.add(&dir.join("a.gcode"), Some("ender".into())).unwrap();
        let b = q.add(&dir.join("b.gcode"), None).unwrap();
        let c = q.add(&dir.join("c.gcode"), Some("prusa".into())).unwrap();
        assert!(q.add(&dir.join("notes.txt"), None).is_err());
        assert_eq!(q.next_for("ender", None).map(|j| j.id), Some(a));
        assert_eq!(q.next_for("prusa", None).map(|j| j.id), Some(b));
        q.shift(c, -5);
        assert_eq!(q.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [c, a, b]);
        assert_eq!(q.next_for("prusa", None).map(|j| j.id), Some(c));
        q.take(c);
        q.assign(a, None);
        assert_eq!(q.next_for("prusa", None).map(|j| j.id), Some(a));
        let path = dir.join("queue.json");
        q.save(&path).unwrap();
        assert_eq!(Queue::load(&path).unwrap(), q);
        let d = q.add(&dir.join("c.gcode"), None).unwrap();
        assert!(d > c);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn printers_only_take_the_filament_they_have_loaded() {
        let dir = scratch("filament");
        let body = |f: &str| format!("G28\nG1 X1\n; filament_type = {f}\n");
        std::fs::write(dir.join("petg.gcode"), body("PETG")).unwrap();
        std::fs::write(dir.join("pla.gcode"), body("PLA")).unwrap();
        std::fs::write(dir.join("any.gcode"), "G28\n").unwrap();
        let mut q = Queue::default();
        let petg = q.add(&dir.join("petg.gcode"), None).unwrap();
        let pla = q.add(&dir.join("pla.gcode"), None).unwrap();
        let any = q.add(&dir.join("any.gcode"), None).unwrap();
        assert_eq!(q.next_for("a", Some("pla")).map(|j| j.id), Some(pla));
        assert_eq!(q.next_for("b", Some("PETG")).map(|j| j.id), Some(petg));
        assert_eq!(q.next_for("c", None).map(|j| j.id), Some(petg));
        assert_eq!(q.next_for("d", Some("TPU")).map(|j| j.id), Some(any));
        assert_eq!(q.waiting_for_filament("d", Some("TPU")), ["PETG", "PLA"]);
        assert!(q.waiting_for_filament("c", None).is_empty());
        q.jobs[0].meta.filament = None;
        assert!(q.fill_in());
        assert_eq!(q.jobs[0].meta.filament.as_deref(), Some("PETG"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_appends() {
        let dir = scratch("history");
        let path = dir.join("history.jsonl");
        let job = Job {
            id: 1,
            name: "part".into(),
            path: "part.gcode".into(),
            printer: None,
            added: 0,
            meta: gcode::Meta { filament_g: Some(12.0), ..Default::default() },
            card: None,
        };
        append(&path, &Record::new(&job, "Ender", 100, Outcome::Done, "")).unwrap();
        append(&path, &Record::new(&job, "Ender", 200, Outcome::Cancelled, "cancelled")).unwrap();
        let h = history(&path);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].filament_g, Some(12.0));
        assert_eq!(h[1].filament_g, None);
        assert_eq!(h[1].reason, "cancelled");
        std::fs::remove_dir_all(&dir).ok();
    }
}
