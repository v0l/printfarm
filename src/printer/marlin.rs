use super::mesh::{GridReader, Mesh, grid_title};
use super::{Dir, Event, Heater, Job, Outcome, Printer, Progress, State, Status};
use crate::gcode;
use serialport::SerialPort;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant, SystemTime};

const READ_TIMEOUT: Duration = Duration::from_millis(20);
const PROBE_EVERY: Duration = Duration::from_secs(2);
const PROBE_TRIES: u32 = 4;
const POLL_EVERY: Duration = Duration::from_secs(2);
const QUIET_POLL: Duration = Duration::from_secs(10);
const POKE_AFTER: Duration = Duration::from_secs(15);
const RETRY_AFTER: Duration = Duration::from_secs(3);
const GIVE_UP_RETRY: Duration = Duration::from_secs(10);
const KEEP_SENT: usize = 64;

pub const BAUDS: [u32; 7] = [115200, 250000, 1000000, 500000, 230400, 57600, 2000000];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Setup,
    Manual,
    Poll,
    Job,
    Final,
}

enum Queued {
    Plain(String),
    Resync,
}

enum Handshake {
    Probing { last: Option<Instant>, tries: u32 },
    Ready,
}

struct Running {
    progress: Progress,
    reader: BufReader<File>,
    draining: bool,
}

pub struct Marlin {
    path: String,
    baud: u32,
    link: Option<Box<dyn SerialPort>>,
    status: Status,
    rx: Vec<u8>,
    handshake: Handshake,
    next_n: u32,
    in_flight: Option<Kind>,
    sent: VecDeque<(u32, String, Kind)>,
    resend: VecDeque<(u32, String, Kind)>,
    resend_from: Option<u32>,
    manual: VecDeque<Queued>,
    job: Option<Running>,
    autoreport: bool,
    last_rx: Instant,
    last_poll: Instant,
    retry_at: Instant,
    poked: bool,
    pending: Vec<Event>,
    grid: Option<GridReader>,
}

impl Marlin {
    pub fn new(path: String, baud: u32) -> Self {
        let now = Instant::now();
        Marlin {
            path,
            baud,
            link: None,
            status: Status::default(),
            rx: Vec::new(),
            handshake: Handshake::Probing { last: None, tries: 0 },
            next_n: 0,
            in_flight: None,
            sent: VecDeque::new(),
            resend: VecDeque::new(),
            resend_from: None,
            manual: VecDeque::new(),
            job: None,
            autoreport: false,
            last_rx: now,
            last_poll: now,
            retry_at: now,
            poked: false,
            pending: Vec::new(),
            grid: None,
        }
    }

    pub fn attached(link: Box<dyn SerialPort>) -> Self {
        let mut m = Marlin::new(String::new(), 0);
        m.attach(link);
        m
    }

    fn attach(&mut self, link: Box<dyn SerialPort>) {
        let now = Instant::now();
        self.link = Some(link);
        self.rx.clear();
        self.handshake = Handshake::Probing { last: None, tries: 0 };
        self.in_flight = None;
        self.sent.clear();
        self.resend.clear();
        self.resend_from = None;
        self.manual.clear();
        self.autoreport = false;
        self.last_rx = now;
        self.last_poll = now;
        self.poked = false;
        self.status.state = Some(State::Connecting);
        self.status.detail = format!("waiting for Marlin on {}", self.where_());
        self.status.heaters.clear();
        self.status.leveling = Default::default();
        self.status.activity = None;
        self.grid = None;
    }

    fn where_(&self) -> String {
        if self.path.is_empty() {
            "the link".into()
        } else {
            format!("{} at {} baud", self.path, self.baud)
        }
    }

    fn connect(&mut self) {
        if Instant::now() < self.retry_at || self.path.is_empty() {
            return;
        }
        match open(&self.path, self.baud) {
            Ok(link) => self.attach(link),
            Err(e) => {
                self.status.state = Some(State::Offline);
                self.status.detail = e;
                self.retry_at = Instant::now() + RETRY_AFTER;
            }
        }
    }

    fn lost(&mut self, why: String, events: &mut Vec<Event>) {
        self.link = None;
        self.end_job(Outcome::Failed, &why, events);
        self.status.state = Some(State::Offline);
        self.status.detail = why;
        self.status.heaters.clear();
        self.retry_at = Instant::now() + RETRY_AFTER;
    }

    fn write(&mut self, text: &str, events: &mut Vec<Event>) -> bool {
        let Some(link) = self.link.as_mut() else { return false };
        let mut line = text.as_bytes().to_vec();
        line.push(b'\n');
        match link.write_all(&line).and_then(|_| link.flush()) {
            Ok(()) => true,
            Err(e) => {
                self.lost(format!("lost {}: {e}", self.where_()), events);
                false
            }
        }
    }

    fn send_numbered(&mut self, cmd: &str, kind: Kind, events: &mut Vec<Event>) {
        let n = self.next_n;
        let text = gcode::numbered(n, cmd);
        if self.write(&text, events) {
            self.next_n += 1;
            self.sent.push_back((n, text, kind));
            while self.sent.len() > KEEP_SENT {
                self.sent.pop_front();
            }
            self.in_flight = Some(kind);
            if kind != Kind::Job && kind != Kind::Poll {
                events.push(Event::Console(Dir::Sent, cmd.to_string()));
            }
        }
    }

    fn read(&mut self, events: &mut Vec<Event>) {
        let Some(link) = self.link.as_mut() else { return };
        let mut buf = [0u8; 1024];
        match link.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => self.rx.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                self.lost(format!("lost {}: {e}", self.where_()), events);
                return;
            }
        }
        while let Some(i) = self.rx.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.rx.drain(..=i).collect();
            let line = String::from_utf8_lossy(&raw).trim().to_string();
            if !line.is_empty() {
                self.last_rx = Instant::now();
                self.poked = false;
                self.line(&line, events);
            }
        }
    }

    fn line(&mut self, line: &str, events: &mut Vec<Event>) {
        if let Some(g) = self.grid.as_mut() {
            if g.feed(line) {
                events.push(Event::Console(Dir::Received, line.to_string()));
                return;
            }
            if let Some(rows) = self.grid.take().and_then(GridReader::finish) {
                let probed = self.status.activity == activity("G29");
                self.status.leveling.mesh = Some(Mesh { rows, read: SystemTime::now(), probed });
            }
        }
        if let Some(keep) = grid_title(line) {
            self.grid = Some(GridReader::new(keep));
            events.push(Event::Console(Dir::Received, line.to_string()));
            return;
        }
        let temps = temperatures(line);
        let reported = !temps.is_empty();
        if reported {
            self.status.heaters = temps;
        }
        if let Some(rest) = line.strip_prefix("ok") {
            if !rest.trim().is_empty() && temperatures(rest).is_empty() {
                events.push(Event::Console(Dir::Received, line.to_string()));
            }
            self.ack(events);
            return;
        }
        if line.starts_with("echo:busy") || (reported && is_report(line)) {
            return;
        }
        events.push(Event::Console(Dir::Received, line.to_string()));
        if let Some(n) = resend_request(line) {
            self.resend_from = Some(n);
        } else if line.starts_with("Error:") || line.starts_with("!!") {
            let lower = line.to_ascii_lowercase();
            if !(lower.contains("line number") || lower.contains("checksum")) && fatal(&lower) {
                self.fault(line.trim_start_matches("Error:").trim().to_string(), events);
            }
        } else if line.contains("FIRMWARE_NAME:") {
            self.status.firmware = field(line, "FIRMWARE_NAME");
            self.status.machine = field(line, "MACHINE_TYPE");
        } else if self.status.leveling.read_state(line) {
        } else if line.starts_with("Cap:AUTOREPORT_TEMP:1") {
            self.autoreport = true;
        } else if let Some(action) = line.strip_prefix("//action:") {
            self.action(action.trim(), events);
        } else if line == "start" && matches!(self.handshake, Handshake::Ready) {
            self.end_job(Outcome::Failed, "the printer restarted", events);
            if let Some(link) = self.link.take() {
                self.attach(link);
            }
        }
    }

    fn action(&mut self, action: &str, events: &mut Vec<Event>) {
        let (verb, rest) = action.split_once(' ').unwrap_or((action, ""));
        match verb {
            "pause" | "paused" => {
                let _ = self.pause();
            }
            "resume" | "resumed" => {
                let _ = self.resume();
            }
            "cancel" => {
                let _ = self.cancel_with(events, "cancelled on the printer");
            }
            "notification" => self.status.message = Some(rest.to_string()),
            _ => {}
        }
    }

    fn ack(&mut self, events: &mut Vec<Event>) {
        if let Handshake::Probing { .. } = self.handshake {
            self.handshake = Handshake::Ready;
            self.status.detail.clear();
            self.next_n = 0;
            self.send_numbered("M110 N0", Kind::Setup, events);
            if self.autoreport {
                self.manual.push_back(Queued::Plain("M155 S2".into()));
            } else {
                self.manual.push_back(Queued::Plain("M105".into()));
            }
            self.manual.push_back(Queued::Plain("M420 V".into()));
            if self.status.state() == State::Connecting {
                self.status.state = Some(State::Idle);
            }
            return;
        }
        let was = self.in_flight.take();
        if let Some(from) = self.resend_from.take() {
            self.resend = self.sent.iter().filter(|(n, _, _)| *n >= from).cloned().collect();
            if self.resend.is_empty() && from != self.next_n {
                self.fault(
                    format!("the printer asked again for line {from}, which is gone"),
                    events,
                );
            }
            return;
        }
        if matches!(was, Some(Kind::Job | Kind::Manual)) {
            self.status.activity = None;
        }
        if was == Some(Kind::Final) {
            self.end_job(Outcome::Done, "", events);
        }
    }

    fn fault(&mut self, why: String, events: &mut Vec<Event>) {
        self.end_job(Outcome::Failed, &why, events);
        self.manual.clear();
        self.resend.clear();
        self.status.state = Some(State::Fault);
        self.status.detail = why;
    }

    fn end_job(&mut self, outcome: Outcome, reason: &str, events: &mut Vec<Event>) {
        if let Some(run) = self.job.take() {
            events.push(Event::Finished {
                job: run.progress.job,
                outcome,
                started: run.progress.started,
                reason: reason.to_string(),
            });
            if self.status.state().busy() {
                self.status.state = Some(State::Idle);
            }
        }
        self.status.progress = None;
        self.status.activity = None;
    }

    fn next_job_line(&mut self) -> Option<(String, Kind)> {
        let run = self.job.as_mut()?;
        if run.draining {
            return None;
        }
        let mut line = String::new();
        loop {
            line.clear();
            match run.reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    run.draining = true;
                    run.progress.done = run.progress.total;
                    return Some(("M400".into(), Kind::Final));
                }
                Ok(n) => {
                    run.progress.done += n as u64;
                    if gcode::is_layer_change(&line) {
                        run.progress.layer += 1;
                    }
                    if let Some(cmd) = gcode::command(&line) {
                        if let Some(text) = cmd.strip_prefix("M117 ") {
                            self.status.message = Some(text.trim().to_string());
                        }
                        return Some((cmd.to_string(), Kind::Job));
                    }
                }
            }
        }
    }

    fn pump(&mut self, events: &mut Vec<Event>) {
        if self.in_flight.is_some() || !matches!(self.handshake, Handshake::Ready) {
            return;
        }
        if let Some((n, text, kind)) = self.resend.pop_front() {
            if self.write(&text, events) {
                self.in_flight = Some(kind);
                events.push(Event::Console(Dir::Note, format!("resent line {n}")));
            }
            return;
        }
        if let Some(q) = self.manual.pop_front() {
            match q {
                Queued::Plain(cmd) => {
                    self.status.activity = activity(&cmd);
                    self.send_numbered(&cmd, Kind::Manual, events);
                }
                Queued::Resync => {
                    let n = self.next_n;
                    self.send_numbered(&format!("M110 N{n}"), Kind::Setup, events);
                }
            }
            return;
        }
        let quiet = self.last_rx.elapsed() > QUIET_POLL;
        if (!self.autoreport && self.last_poll.elapsed() > POLL_EVERY) || quiet {
            self.last_poll = Instant::now();
            self.send_numbered("M105", Kind::Poll, events);
            return;
        }
        if self.status.state() == State::Printing
            && let Some((cmd, kind)) = self.next_job_line()
        {
            self.status.activity = activity(&cmd);
            self.send_numbered(&cmd, kind, events);
            if let (Some(run), Some(p)) = (&self.job, self.status.progress.as_mut()) {
                p.done = run.progress.done;
                p.layer = run.progress.layer;
            }
        }
    }

    fn probe(&mut self, events: &mut Vec<Event>) {
        let Handshake::Probing { last, tries } = self.handshake else { return };
        let due = match last {
            None => self.last_rx.elapsed() > Duration::from_millis(500),
            Some(at) => at.elapsed() > PROBE_EVERY,
        };
        if !due {
            return;
        }
        if tries >= PROBE_TRIES {
            let why = format!("no answer from {}", self.where_());
            self.link = None;
            self.status.state = Some(State::Offline);
            self.status.detail = why;
            self.retry_at = Instant::now() + GIVE_UP_RETRY;
            return;
        }
        if self.write("M115", events) {
            self.handshake = Handshake::Probing { last: Some(Instant::now()), tries: tries + 1 };
        }
    }

    fn cancel_with(&mut self, events: &mut Vec<Event>, reason: &str) -> Result<(), String> {
        if self.job.is_none() {
            return Err("nothing is printing".into());
        }
        self.end_job(Outcome::Cancelled, reason, events);
        self.resend.clear();
        self.resend_from = None;
        self.manual.clear();
        if self.write("M108", events) {
            events.push(Event::Console(Dir::Sent, "M108".into()));
        }
        self.manual.push_back(Queued::Resync);
        for cmd in ["M104 S0", "M140 S0", "M107"] {
            self.manual.push_back(Queued::Plain(cmd.into()));
        }
        self.status.state = Some(State::Idle);
        Ok(())
    }
}

impl Printer for Marlin {
    fn status(&self) -> &Status {
        &self.status
    }

    fn step(&mut self, events: &mut Vec<Event>) {
        events.append(&mut self.pending);
        if self.link.is_none() {
            self.connect();
            if self.link.is_none() {
                std::thread::sleep(READ_TIMEOUT);
                return;
            }
        }
        self.probe(events);
        self.read(events);
        if self.link.is_none() {
            return;
        }
        if self.in_flight.is_some() && self.last_rx.elapsed() > POKE_AFTER && !self.poked {
            self.poked = true;
            events.push(Event::Console(Dir::Note, "no answer for 15 s, asking again".into()));
            self.write("M105", events);
        }
        self.pump(events);
    }

    fn start(&mut self, job: Job) -> Result<(), String> {
        if self.status.state() != State::Idle {
            return Err(format!("the printer is {}", self.status.state().label()));
        }
        let file = File::open(&job.path).map_err(|e| format!("{}: {e}", job.path.display()))?;
        let total = file.metadata().map(|m| m.len()).unwrap_or(0);
        let progress = Progress {
            job,
            done: 0,
            total,
            layer: 0,
            started: SystemTime::now(),
            paused: Duration::ZERO,
            paused_since: None,
        };
        self.status.progress = Some(progress.clone());
        self.status.message = None;
        self.job = Some(Running {
            progress,
            reader: BufReader::with_capacity(1 << 16, file),
            draining: false,
        });
        self.status.state = Some(State::Printing);
        Ok(())
    }

    fn pause(&mut self) -> Result<(), String> {
        if self.status.state() != State::Printing {
            return Err("nothing is printing".into());
        }
        self.status.state = Some(State::Paused);
        let now = SystemTime::now();
        for p in self.job.iter_mut().map(|r| &mut r.progress).chain(self.status.progress.as_mut()) {
            p.paused_since = Some(now);
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        if self.status.state() != State::Paused {
            return Err("the printer is not paused".into());
        }
        self.status.state = Some(State::Printing);
        let now = SystemTime::now();
        for p in self.job.iter_mut().map(|r| &mut r.progress).chain(self.status.progress.as_mut()) {
            if let Some(since) = p.paused_since.take() {
                p.paused += now.duration_since(since).unwrap_or_default();
            }
        }
        Ok(())
    }

    fn cancel(&mut self) -> Result<(), String> {
        let mut events = Vec::new();
        let r = self.cancel_with(&mut events, "cancelled");
        self.pending.extend(events);
        r
    }

    fn gcode(&mut self, line: &str) -> Result<(), String> {
        if self.link.is_none() {
            return Err("the printer is not connected".into());
        }
        for cmd in line.lines().filter_map(gcode::command) {
            self.manual.push_back(Queued::Plain(cmd.to_string()));
        }
        Ok(())
    }

    fn settled(&self) -> bool {
        self.link.is_none()
            || (self.manual.is_empty() && self.resend.is_empty() && self.in_flight.is_none())
    }

    fn reconnect(&mut self) {
        let mut events = Vec::new();
        self.end_job(Outcome::Failed, "reconnected", &mut events);
        self.pending.extend(events);
        self.link = None;
        self.status = Status::default();
        self.retry_at = Instant::now();
    }
}

pub fn open(path: &str, baud: u32) -> Result<Box<dyn SerialPort>, String> {
    serialport::new(path, baud).timeout(READ_TIMEOUT).open().map_err(|e| match e.kind() {
        serialport::ErrorKind::NoDevice
        | serialport::ErrorKind::Io(std::io::ErrorKind::NotFound) => {
            format!("{path} is not there. Plug the printer in and turn it on.")
        }
        _ => format!("cannot open {path}: {e}"),
    })
}

fn activity(cmd: &str) -> Option<&'static str> {
    match cmd.split_whitespace().next()? {
        "M109" => Some("heating the hot end"),
        "M190" => Some("heating the bed"),
        "M191" => Some("heating the chamber"),
        "G28" => Some("homing"),
        "G29" => Some("probing the bed"),
        "M600" => Some("waiting for a filament change"),
        _ => None,
    }
}

fn resend_request(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("Resend:").or_else(|| line.strip_prefix("rs "))?;
    rest.trim().trim_start_matches('N').split_whitespace().next()?.parse().ok()
}

fn fatal(lower: &str) -> bool {
    ["kill", "halted", "thermal", "mintemp", "maxtemp", "heating failed", "stopped"]
        .iter()
        .any(|w| lower.contains(w))
}

fn is_report(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or("");
    first.starts_with("T:") || first.starts_with("T0:") || first.starts_with("B:")
}

pub fn field(line: &str, key: &str) -> Option<String> {
    let start = line.find(&format!("{key}:"))? + key.len() + 1;
    let rest = &line[start..];
    let end = rest
        .match_indices(' ')
        .map(|(i, _)| i)
        .find(|i| {
            let word = rest[i + 1..].split(' ').next().unwrap_or("");
            word.split_once(':').is_some_and(|(k, _)| {
                k.len() > 1 && k.chars().all(|c| c.is_ascii_uppercase() || c == '_')
            })
        })
        .unwrap_or(rest.len());
    let v = rest[..end].trim();
    (!v.is_empty()).then(|| v.to_string())
}

pub fn temperatures(line: &str) -> Vec<Heater> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut out: Vec<Heater> = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let Some((key, value)) = word.split_once(':') else { continue };
        let name = match key {
            "T" => "hotend".to_string(),
            "B" => "bed".to_string(),
            "C" => "chamber".to_string(),
            k if k.len() == 2
                && k.starts_with('T')
                && k[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                format!("tool {}", &k[1..])
            }
            _ => continue,
        };
        let (actual, target) = match value.split_once('/') {
            Some((a, t)) => (a.parse().ok(), t.parse().ok()),
            None => {
                let next = words.get(i + 1).and_then(|w| w.strip_prefix('/'));
                (value.parse().ok(), next.and_then(|t| t.parse().ok()))
            }
        };
        let Some(actual) = actual else { continue };
        out.push(Heater { name, actual, target: target.unwrap_or(0.0) });
    }
    if out.iter().any(|h| h.name.starts_with("tool ")) {
        out.retain(|h| h.name != "hotend");
    }
    out
}

pub struct Detected {
    pub baud: u32,
    pub firmware: Option<String>,
    pub machine: Option<String>,
}

pub fn detect(path: &str) -> Result<Detected, String> {
    for baud in BAUDS {
        let mut link = open(path, baud)?;
        let started = Instant::now();
        let mut asked = None::<Instant>;
        let mut rx = Vec::new();
        let mut buf = [0u8; 512];
        while started.elapsed() < Duration::from_millis(2500) {
            if asked.is_none_or(|a| a.elapsed() > Duration::from_millis(1200)) {
                let _ = link.write_all(b"\nM115\n");
                asked = Some(Instant::now());
            }
            if let Ok(n) = link.read(&mut buf) {
                rx.extend_from_slice(&buf[..n]);
            }
            let text = String::from_utf8_lossy(&rx);
            if let Some(line) = text.lines().find(|l| l.contains("FIRMWARE_NAME:")) {
                return Ok(Detected {
                    baud,
                    firmware: field(line, "FIRMWARE_NAME"),
                    machine: field(line, "MACHINE_TYPE"),
                });
            }
            if text.lines().any(|l| l.trim() == "ok") {
                return Ok(Detected { baud, firmware: None, machine: None });
            }
        }
    }
    Err(format!("no Marlin on {path} at {} baud", BAUDS.map(|b| b.to_string()).join(", ")))
}

pub struct PortInfo {
    pub path: String,
    pub label: String,
}

pub fn ports() -> Vec<PortInfo> {
    let mut out = Vec::new();
    let Ok(found) = serialport::available_ports() else { return out };
    for p in found {
        let label = match &p.port_type {
            serialport::SerialPortType::UsbPort(u) => {
                let product =
                    u.product.as_deref().map(str::trim).unwrap_or("USB serial").to_string();
                format!("{product} ({:04x}:{:04x})", u.vid, u.pid)
            }
            serialport::SerialPortType::BluetoothPort => "Bluetooth".into(),
            serialport::SerialPortType::PciPort => "PCI".into(),
            serialport::SerialPortType::Unknown => "serial".into(),
        };
        if matches!(
            p.port_type,
            serialport::SerialPortType::Unknown | serialport::SerialPortType::PciPort
        ) && cfg!(target_os = "linux")
        {
            continue;
        }
        let path = stable_path(&p.port_name).unwrap_or(p.port_name.clone());
        out.push(PortInfo { label: format!("{label}, {}", p.port_name), path });
    }
    out
}

fn stable_path(dev: &str) -> Option<String> {
    let real = std::fs::canonicalize(dev).ok()?;
    let dir = std::fs::read_dir("/dev/serial/by-path").ok()?;
    let mut links: Vec<String> = dir
        .flatten()
        .filter(|e| std::fs::canonicalize(e.path()).ok().as_ref() == Some(&real))
        .map(|e| e.path().display().to_string())
        .collect();
    links.sort_by_key(|l| l.contains("usbv2"));
    links.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_temperature_reports() {
        let t = temperatures("ok T:210.2 /210.0 B:60.1 /60.0 @:127 B@:0");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], Heater { name: "hotend".into(), actual: 210.2, target: 210.0 });
        assert_eq!(t[1], Heater { name: "bed".into(), actual: 60.1, target: 60.0 });
        let t = temperatures(" T:21.41 /0.00 B:21.62 /0.00 @:0 B@:0");
        assert_eq!(t[0].actual, 21.41);
        let t = temperatures("T:150.00/150.00 B:64.9/65.0 W:?");
        assert_eq!(t[1].target, 65.0);
        let t = temperatures("T:200 /200 B:60 /60 T0:200 /200 T1:25 /0");
        assert_eq!(
            t.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            ["bed", "tool 0", "tool 1"]
        );
        assert!(temperatures("echo:busy: processing").is_empty());
    }

    #[test]
    fn reads_firmware_fields() {
        let line = "FIRMWARE_NAME:Marlin bugfix-2.0.x (Jan 21 2021 17:17:29) SOURCE_CODE_URL:github.com/MarlinFirmware/Marlin PROTOCOL_VERSION:1.0 MACHINE_TYPE:Ender-3 Pro EXTRUDER_COUNT:1 UUID:cede2a2f";
        assert_eq!(
            field(line, "FIRMWARE_NAME").as_deref(),
            Some("Marlin bugfix-2.0.x (Jan 21 2021 17:17:29)")
        );
        assert_eq!(field(line, "MACHINE_TYPE").as_deref(), Some("Ender-3 Pro"));
        assert_eq!(field(line, "UUID").as_deref(), Some("cede2a2f"));
    }

    #[test]
    fn reads_resend_requests() {
        assert_eq!(resend_request("Resend: 42"), Some(42));
        assert_eq!(resend_request("Resend:N42"), Some(42));
        assert_eq!(resend_request("rs N7"), Some(7));
        assert_eq!(resend_request("ok"), None);
    }
}
