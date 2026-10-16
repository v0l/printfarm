use crate::config::PrinterConfig;
use crate::printer::{self, Dir, Event, Job, Printer, Status};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

const CONSOLE: usize = 500;
const SAMPLES: usize = 900;
const SAMPLE_EVERY: Duration = Duration::from_secs(2);
const PUBLISH_EVERY: Duration = Duration::from_millis(200);

pub enum Control {
    Start(Job),
    Pause,
    Resume,
    Cancel,
    Gcode(String),
    Reconnect,
    Quit,
}

pub struct Sample {
    pub at: SystemTime,
    pub heaters: Vec<(String, f32, f32)>,
}

#[derive(Default)]
pub struct Shared {
    pub status: Status,
    pub console: VecDeque<(Dir, String)>,
    pub samples: VecDeque<Sample>,
    pub refused: Option<String>,
}

pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub struct Station {
    pub config: PrinterConfig,
    pub shared: Arc<Mutex<Shared>>,
    tx: Sender<Control>,
    thread: Option<JoinHandle<()>>,
}

impl Station {
    pub fn spawn(config: PrinterConfig, events: Sender<(String, Event)>, wake: Wake) -> Station {
        let backend = printer::open(&config);
        Station::with(config, backend, events, wake)
    }

    pub fn with(
        config: PrinterConfig,
        backend: Box<dyn Printer>,
        events: Sender<(String, Event)>,
        wake: Wake,
    ) -> Station {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = channel();
        let id = config.id.clone();
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("printer {id}"))
            .spawn(move || run(id, backend, rx, s, events, wake))
            .ok();
        Station { config, shared, tx, thread }
    }

    pub fn send(&self, c: Control) {
        let _ = self.tx.send(c);
    }

    pub fn status(&self) -> Status {
        self.shared.lock().map(|s| s.status.clone()).unwrap_or_default()
    }
}

impl Drop for Station {
    fn drop(&mut self) {
        let _ = self.tx.send(Control::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const SETTLE: Duration = Duration::from_secs(3);

fn settle(backend: &mut dyn Printer, out: &mut Vec<Event>) {
    let t = Instant::now();
    while !backend.settled() && t.elapsed() < SETTLE {
        backend.step(out);
    }
}

fn run(
    id: String,
    mut backend: Box<dyn Printer>,
    rx: Receiver<Control>,
    shared: Arc<Mutex<Shared>>,
    events: Sender<(String, Event)>,
    wake: Wake,
) {
    let mut out = Vec::new();
    let mut published = Status::default();
    let mut last_publish = Instant::now() - PUBLISH_EVERY;
    let mut last_sample = Instant::now() - SAMPLE_EVERY;
    loop {
        let mut refused = None;
        loop {
            let c = match rx.try_recv() {
                Ok(c) => c,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            };
            let r = match c {
                Control::Quit => {
                    settle(backend.as_mut(), &mut out);
                    return;
                }
                Control::Start(job) => {
                    let j = job.clone();
                    match backend.start(job) {
                        Ok(()) => out.push(Event::Started(j)),
                        Err(e) => out.push(Event::Refused(j, e)),
                    }
                    Ok(())
                }
                Control::Pause => backend.pause(),
                Control::Resume => backend.resume(),
                Control::Cancel => backend.cancel(),
                Control::Gcode(g) => backend.gcode(&g),
                Control::Reconnect => {
                    backend.reconnect();
                    Ok(())
                }
            };
            if let Err(e) = r {
                refused = Some(e);
            }
        }
        backend.step(&mut out);
        let status = backend.status();
        let state_changed = status.state != published.state
            || status.progress.is_none() != published.progress.is_none();
        let publish =
            *status != published && (state_changed || last_publish.elapsed() >= PUBLISH_EVERY);
        let sample = last_sample.elapsed() >= SAMPLE_EVERY && !status.heaters.is_empty();
        if out.is_empty() && refused.is_none() && !sample && !publish {
            continue;
        }
        if let Ok(mut s) = shared.lock() {
            if publish {
                s.status = status.clone();
                published = status.clone();
                last_publish = Instant::now();
            }
            if sample {
                last_sample = Instant::now();
                s.samples.push_back(Sample {
                    at: SystemTime::now(),
                    heaters: status
                        .heaters
                        .iter()
                        .map(|h| (h.name.clone(), h.actual, h.target))
                        .collect(),
                });
                while s.samples.len() > SAMPLES {
                    s.samples.pop_front();
                }
            }
            if refused.is_some() {
                s.refused = refused;
            }
            for e in &out {
                if let Event::Console(dir, text) = e {
                    s.console.push_back((*dir, text.clone()));
                }
            }
            while s.console.len() > CONSOLE {
                s.console.pop_front();
            }
        }
        for e in out.drain(..) {
            if !matches!(e, Event::Console(..)) {
                let _ = events.send((id.clone(), e));
            }
        }
        wake();
    }
}
