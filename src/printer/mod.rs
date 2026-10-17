pub mod marlin;
pub mod mesh;
#[cfg(unix)]
pub mod sim;

use crate::config::{Link, PrinterConfig};
use crate::gcode::Meta;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Offline,
    Connecting,
    Idle,
    Printing,
    Paused,
    Fault,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Offline => "offline",
            State::Connecting => "connecting",
            State::Idle => "ready",
            State::Printing => "printing",
            State::Paused => "paused",
            State::Fault => "fault",
        }
    }

    pub fn busy(self) -> bool {
        matches!(self, State::Printing | State::Paused)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Heater {
    pub name: String,
    pub actual: f32,
    pub target: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub printer: Option<String>,
    pub added: u64,
    #[serde(default)]
    pub meta: Meta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub job: Job,
    pub done: u64,
    pub total: u64,
    pub layer: usize,
    pub started: SystemTime,
    pub paused: Duration,
    pub paused_since: Option<SystemTime>,
    pub sending: bool,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 { 0.0 } else { (self.done as f64 / self.total as f64) as f32 }
    }

    pub fn elapsed(&self, now: SystemTime) -> Duration {
        let end = self.paused_since.unwrap_or(now);
        end.duration_since(self.started).unwrap_or_default().saturating_sub(self.paused)
    }

    pub fn remaining(&self, now: SystemTime) -> Option<Duration> {
        let f = self.fraction() as f64;
        let elapsed = self.elapsed(now).as_secs_f64();
        let by_rate = (f > 0.02).then(|| elapsed / f - elapsed);
        let by_slicer =
            self.job.meta.estimate().filter(|_| !self.sending).map(|e| e.as_secs_f64() * (1.0 - f));
        let secs = match (by_slicer, by_rate) {
            (Some(s), Some(r)) => s * (1.0 - f) + r * f,
            (Some(s), None) => s,
            (None, Some(r)) => r,
            (None, None) => return None,
        };
        Some(Duration::from_secs_f64(secs.max(0.0)))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Foreign {
    pub file: String,
    pub done: u64,
    pub total: u64,
    pub printing: bool,
}

impl Foreign {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 { 0.0 } else { (self.done as f64 / self.total as f64) as f32 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CardFile {
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub state: Option<State>,
    pub detail: String,
    pub firmware: Option<String>,
    pub machine: Option<String>,
    pub heaters: Vec<Heater>,
    pub progress: Option<Progress>,
    pub message: Option<String>,
    pub activity: Option<&'static str>,
    pub leveling: mesh::Leveling,
    pub foreign: Option<Foreign>,
    pub card: Option<Vec<CardFile>>,
}

impl Status {
    pub fn state(&self) -> State {
        self.state.unwrap_or(State::Offline)
    }

    pub fn heater(&self, name: &str) -> Option<&Heater> {
        self.heaters.iter().find(|h| h.name == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Done,
    Cancelled,
    Failed,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Cancelled => "cancelled",
            Outcome::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Sent,
    Received,
    Note,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Console(Dir, String),
    Started(Job),
    OnPrinter(Job),
    Interrupted(Job, String),
    Refused(Job, String),
    Finished { job: Job, outcome: Outcome, started: SystemTime, reason: String },
}

pub trait Printer: Send {
    fn status(&self) -> &Status;
    fn step(&mut self, events: &mut Vec<Event>);
    fn start(&mut self, job: Job) -> Result<(), String>;
    fn pause(&mut self) -> Result<(), String>;
    fn resume(&mut self) -> Result<(), String>;
    fn cancel(&mut self) -> Result<(), String>;
    fn gcode(&mut self, line: &str) -> Result<(), String>;
    fn settled(&self) -> bool;
    fn reconnect(&mut self);
    fn adopt(&mut self, job: Job, started: SystemTime, on_printer: bool);
    fn leave(&mut self);
}

pub fn open(config: &PrinterConfig) -> Box<dyn Printer> {
    match &config.link {
        Link::MarlinUsb { port, baud } => Box::new(marlin::Marlin::new(port.clone(), *baud)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(done: u64, estimate: Option<u64>, elapsed: u64) -> Progress {
        let now = SystemTime::now();
        Progress {
            job: Job {
                id: 1,
                name: "part".into(),
                path: "part.gcode".into(),
                printer: None,
                added: 0,
                meta: Meta { estimate_s: estimate, ..Meta::default() },
                card: None,
            },
            done,
            total: 100,
            layer: 0,
            started: now - Duration::from_secs(elapsed),
            paused: Duration::ZERO,
            paused_since: None,
            sending: false,
        }
    }

    #[test]
    fn remaining_leans_on_slicer_early_and_rate_late() {
        let now = SystemTime::now();
        let early = progress(1, Some(1000), 10).remaining(now).unwrap().as_secs();
        assert!((985..=995).contains(&early), "{early}");
        let late = progress(90, Some(1000), 2000).remaining(now).unwrap().as_secs();
        assert!((200..=225).contains(&late), "{late}");
        assert!(progress(1, None, 10).remaining(now).is_none());
    }

    #[test]
    fn paused_time_is_not_counted() {
        let mut p = progress(50, None, 100);
        p.paused = Duration::from_secs(40);
        let e = p.elapsed(SystemTime::now()).as_secs();
        assert!((59..=61).contains(&e), "{e}");
    }
}
