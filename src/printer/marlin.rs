use super::mesh::{GridReader, Mesh, grid_title};
use super::{
    CardFile, Dir, Event, Foreign, Heater, Job, Outcome, Printer, Progress, State, Status,
};
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
const SD_POLL_BUSY: Duration = Duration::from_secs(3);
const CARD_POLL: Duration = Duration::from_secs(30);
const SD_POLL_IDLE: Duration = Duration::from_secs(10);
const QUIET_POLL: Duration = Duration::from_secs(10);
const HEAT_EXPIRY: Duration = Duration::from_secs(3);
const POKE_AFTER: Duration = Duration::from_secs(15);
const RETRY_AFTER: Duration = Duration::from_secs(3);
const GIVE_UP_RETRY: Duration = Duration::from_secs(10);
const KEEP_SENT: usize = 64;

pub const BAUDS: [u32; 7] = [115200, 250000, 1000000, 500000, 230400, 57600, 2000000];

pub fn sd_name(job: u64) -> String {
    format!("PF{:06}.GCO", job % 1_000_000)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Setup,
    Manual,
    Poll,
    Open,
    Upload,
    Close,
}

enum Queued {
    Plain(String),
    Kinded(String, Kind),
    Resync,
}

enum Handshake {
    Probing { last: Option<Instant>, tries: u32 },
    Ready,
}

enum Phase {
    Selecting,
    Opening(BufReader<File>),
    Sending(BufReader<File>),
    Closing,
    OnPrinter,
}

struct Running {
    progress: Progress,
    file: String,
    phase: Phase,
    layers: Vec<u64>,
    bytes: u64,
    card: bool,
}

impl Running {
    fn uploading(&self) -> bool {
        !matches!(self.phase, Phase::OnPrinter)
    }
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
    polls: VecDeque<&'static str>,
    job: Option<Running>,
    autoreport: bool,
    sd_cap: Option<bool>,
    sd_file: Option<Option<String>>,
    away: bool,
    cleanup: Vec<String>,
    last_rx: Instant,
    attached_at: Instant,
    last_poll: Instant,
    last_sd_poll: Instant,
    last_heating: Option<Instant>,
    retry_at: Instant,
    poked: bool,
    pending: Vec<Event>,
    grid: Option<GridReader>,
    listing: Option<Vec<CardFile>>,
    last_card_poll: Instant,
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
            polls: VecDeque::new(),
            job: None,
            autoreport: false,
            sd_cap: None,
            sd_file: None,
            away: false,
            cleanup: Vec::new(),
            last_rx: now,
            attached_at: now,
            last_poll: now,
            last_sd_poll: now,
            last_heating: None,
            retry_at: now,
            poked: false,
            pending: Vec::new(),
            grid: None,
            listing: None,
            last_card_poll: now,
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
        self.polls.clear();
        self.autoreport = false;
        self.sd_cap = None;
        self.sd_file = None;
        self.last_rx = now;
        self.attached_at = now;
        self.last_poll = now;
        self.last_heating = None;
        self.poked = false;
        self.status.state = Some(State::Connecting);
        self.status.detail = format!("waiting for Marlin on {}", self.where_());
        self.status.heaters.clear();
        self.status.leveling = Default::default();
        self.status.activity = None;
        self.status.foreign = None;
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
        if self.job.as_ref().is_some_and(Running::uploading) {
            self.interrupt("the printer went away while it was being sent", events);
        }
        if self.job.is_some() {
            self.away = true;
        }
        self.status.state = Some(State::Offline);
        self.status.detail = why;
        self.status.heaters.clear();
        self.status.foreign = None;
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
            if kind != Kind::Upload && kind != Kind::Poll {
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
        if line.starts_with("Begin file list") {
            self.listing = Some(Vec::new());
            return;
        }
        if let Some(list) = self.listing.as_mut() {
            if line.starts_with("End file list") {
                self.status.card = self.listing.take();
                return;
            }
            if let Some(file) = card_file(line) {
                list.push(file);
                return;
            }
        }
        if line.contains("No SD card") || line.contains("SD init fail") {
            self.status.card = None;
        }
        if let Some(g) = self.grid.as_mut() {
            if g.feed(line) {
                events.push(Event::Console(Dir::Received, line.to_string()));
                return;
            }
            if let Some(rows) = self.grid.take().and_then(GridReader::finish) {
                let before =
                    self.status.leveling.mesh.take().filter(|m| m.rows == rows && m.probed);
                let mesh = match before {
                    Some(m) if self.status.activity != activity("G29") => m,
                    _ => Mesh {
                        rows,
                        read: SystemTime::now(),
                        probed: self.status.activity == activity("G29"),
                    },
                };
                self.status.leveling.mesh = Some(mesh);
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
            if self.job.as_ref().is_some_and(|r| !r.uploading()) && line.contains(" W:") {
                self.last_heating = Some(Instant::now());
                if self.status.activity.is_none() {
                    self.status.activity = Some("heating");
                }
            }
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
        if self.in_flight != Some(Kind::Poll) {
            events.push(Event::Console(Dir::Received, line.to_string()));
        }
        let text = line.trim_start_matches("echo:");
        if !matches!(self.handshake, Handshake::Ready) {
            if line.contains("FIRMWARE_NAME:") {
                self.status.firmware = field(line, "FIRMWARE_NAME");
                self.status.machine = field(line, "MACHINE_TYPE");
            } else if line.starts_with("Cap:AUTOREPORT_TEMP:1") {
                self.autoreport = true;
            } else if let Some(v) = line.strip_prefix("Cap:SDCARD:") {
                self.sd_cap = Some(v.trim() == "1");
            }
            return;
        }
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
        } else if let Some(v) = line.strip_prefix("Cap:SDCARD:") {
            self.sd_cap = Some(v.trim() == "1");
        } else if let Some(rest) = text.strip_prefix("Current file:") {
            let name = rest.split_whitespace().next().unwrap_or("");
            self.sd_file = Some((!name.is_empty() && name != "(no").then(|| name.to_string()));
        } else if let Some(rest) = text.strip_prefix("SD printing byte") {
            let pos = rest
                .trim()
                .split_once('/')
                .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)));
            self.reconcile(pos.or(Some((0, 0))), events);
        } else if text.starts_with("Not SD printing") {
            self.reconcile(None, events);
        } else if text.starts_with("Done printing file") {
            self.sd_file = Some(None);
            self.reconcile(None, events);
        } else if text.contains("open failed") {
            self.open_failed(text, events);
        } else if text.contains("No SD card")
            || text.contains("SD init fail")
            || text.contains("No media")
        {
            if self.job.as_ref().is_some_and(Running::uploading) {
                self.manual.clear();
                self.interrupt("there is no SD card in the printer", events);
            }
        } else if let Some(action) = line.strip_prefix("//action:") {
            self.action(action.trim(), events);
        } else if line == "start" && matches!(self.handshake, Handshake::Ready) {
            self.drop_job("the printer restarted", events);
            if let Some(link) = self.link.take() {
                self.attach(link);
            }
        }
    }

    fn open_failed(&mut self, text: &str, events: &mut Vec<Event>) {
        let Some(run) = &self.job else { return };
        if !text.to_ascii_lowercase().contains(&run.file.to_ascii_lowercase()) {
            return;
        }
        match run.phase {
            Phase::Selecting => {
                self.manual.clear();
                if let Some(mut run) = self.job.take() {
                    run.progress.job.card = None;
                    let why = format!(
                        "{} is not on the printer's card, it will be sent over serial instead",
                        run.file
                    );
                    events.push(Event::Interrupted(run.progress.job, why));
                }
                self.status.progress = None;
                self.status.state = Some(State::Idle);
            }
            Phase::Opening(_) => {
                self.manual.clear();
                self.interrupt("the printer could not write the file to its SD card", events);
            }
            Phase::OnPrinter => self.end_job(
                Outcome::Failed,
                "the printer could not open the file on its SD card",
                events,
            ),
            _ => {}
        }
    }

    fn reconcile(&mut self, printing: Option<(u64, u64)>, events: &mut Vec<Event>) {
        let current = self.sd_file.take();
        let ours = self.job.as_ref().filter(|r| !r.uploading()).map(|r| r.file.clone());
        if self.job.as_ref().is_some_and(Running::uploading) {
            return;
        }
        let is_ours = |name: &str| ours.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(name));
        let away = std::mem::take(&mut self.away);
        let reason = if away { "finished while printfarm was away" } else { "" };
        match current {
            Some(Some(name)) if is_ours(&name) => self.ours(printing),
            None if ours.is_some() && printing.is_some() => self.ours(printing),
            None if printing.is_none() && self.status.state() == State::Printing => {
                self.away = away;
                self.polls.extend(["M27 C", "M27"]);
            }
            None => self.away = away,
            Some(other) => {
                if ours.is_some() {
                    self.end_job(Outcome::Done, reason, events);
                }
                self.status.foreign = other.map(|file| {
                    let (done, total) = printing.unwrap_or_default();
                    Foreign { file, done, total, printing: printing.is_some() }
                });
                self.status.state = Some(match &self.status.foreign {
                    Some(f) if f.printing => State::Printing,
                    Some(_) => State::Paused,
                    None => State::Idle,
                });
            }
        }
    }

    fn ours(&mut self, printing: Option<(u64, u64)>) {
        self.status.foreign = None;
        self.away = false;
        let now = SystemTime::now();
        let Some(run) = self.job.as_mut() else { return };
        let p = &mut run.progress;
        match printing {
            Some((done, total)) => {
                if total > 0 {
                    p.done = done;
                    p.total = total;
                    let at = (done as f64 / total as f64 * run.bytes as f64) as u64;
                    p.layer = gcode::layer_at(&run.layers, at);
                }
                if let Some(since) = p.paused_since.take() {
                    p.paused += now.duration_since(since).unwrap_or_default();
                }
                self.status.state = Some(State::Printing);
            }
            None => {
                p.paused_since.get_or_insert(now);
                self.status.state = Some(State::Paused);
            }
        }
        self.status.progress = Some(run.progress.clone());
    }

    fn action(&mut self, action: &str, events: &mut Vec<Event>) {
        let (verb, rest) = action.split_once(' ').unwrap_or((action, ""));
        match verb {
            "cancel" => {
                let _ = self.cancel_with(events, "cancelled on the printer");
            }
            "notification" => self.status.message = Some(rest.to_string()),
            _ => {}
        }
    }

    fn ack(&mut self, events: &mut Vec<Event>) {
        if let Handshake::Probing { .. } = self.handshake {
            self.ready(events);
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
        if was == Some(Kind::Manual) {
            self.status.activity = None;
        }
        match was {
            Some(Kind::Open) => {
                if self.job.as_ref().is_some_and(|r| matches!(r.phase, Phase::Selecting)) {
                    self.begin_from_card(events);
                } else if let Some(run) = self.job.as_mut() {
                    run.phase = match std::mem::replace(&mut run.phase, Phase::Closing) {
                        Phase::Opening(reader) => Phase::Sending(reader),
                        other => other,
                    };
                }
            }
            Some(Kind::Close) => self.on_printer(events),
            _ => {}
        }
    }

    fn ready(&mut self, events: &mut Vec<Event>) {
        self.handshake = Handshake::Ready;
        self.status.detail.clear();
        self.next_n = 0;
        self.send_numbered("M110 N0", Kind::Setup, events);
        self.manual.push_back(Queued::Plain("M29".into()));
        if self.autoreport {
            self.manual.push_back(Queued::Plain("M155 S2".into()));
        } else {
            self.manual.push_back(Queued::Plain("M105".into()));
        }
        for file in std::mem::take(&mut self.cleanup) {
            self.manual.push_back(Queued::Plain(format!("M30 {file}")));
        }
        self.manual.push_back(Queued::Plain("M420 V".into()));
        if self.sd_cap != Some(false) {
            self.polls.extend(["M27 C", "M27"]);
            if self.job.is_none() {
                self.polls.push_back("M20");
            }
            self.last_sd_poll = Instant::now();
            self.last_card_poll = Instant::now();
        }
        self.status.state = Some(if self.job.is_some() { State::Printing } else { State::Idle });
    }

    fn on_printer(&mut self, events: &mut Vec<Event>) {
        let Some(run) = self.job.as_mut() else { return };
        run.phase = Phase::OnPrinter;
        let p = &mut run.progress;
        p.sending = false;
        p.done = 0;
        p.layer = 0;
        p.started = SystemTime::now();
        p.paused = Duration::ZERO;
        p.paused_since = None;
        let file = run.file.clone();
        self.status.progress = Some(run.progress.clone());
        self.manual.push_back(Queued::Plain(format!("M23 {file}")));
        self.manual.push_back(Queued::Plain("M24".into()));
        events.push(Event::OnPrinter(run.progress.job.clone()));
    }

    fn begin_from_card(&mut self, events: &mut Vec<Event>) {
        let Some(run) = self.job.as_mut() else { return };
        run.phase = Phase::OnPrinter;
        run.progress.started = SystemTime::now();
        self.status.progress = Some(run.progress.clone());
        self.manual.push_back(Queued::Plain("M24".into()));
        events.push(Event::OnPrinter(run.progress.job.clone()));
    }

    fn fault(&mut self, why: String, events: &mut Vec<Event>) {
        self.drop_job(&why, events);
        self.manual.clear();
        self.resend.clear();
        self.status.state = Some(State::Fault);
        self.status.detail = why;
    }

    fn drop_job(&mut self, why: &str, events: &mut Vec<Event>) {
        if self.job.as_ref().is_some_and(Running::uploading) {
            self.interrupt(why, events);
        } else {
            self.end_job(Outcome::Failed, why, events);
        }
    }

    fn interrupt(&mut self, why: &str, events: &mut Vec<Event>) {
        if let Some(run) = self.job.take() {
            if !run.card {
                self.cleanup.push(run.file);
            }
            events.push(Event::Interrupted(run.progress.job, why.to_string()));
        }
        self.status.progress = None;
        if self.status.state().busy() {
            self.status.state = Some(State::Idle);
        }
    }

    fn end_job(&mut self, outcome: Outcome, reason: &str, events: &mut Vec<Event>) {
        if let Some(run) = self.job.take() {
            if !run.uploading() && !run.card {
                self.manual.push_back(Queued::Plain(format!("M30 {}", run.file)));
            }
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

    fn next_upload_line(&mut self) -> Option<(String, Kind)> {
        let run = self.job.as_mut()?;
        let Phase::Sending(reader) = &mut run.phase else { return None };
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    run.phase = Phase::Closing;
                    run.progress.done = run.progress.total;
                    return Some(("M29".into(), Kind::Close));
                }
                Ok(_) => {
                    if let Some(cmd) = gcode::command(&line) {
                        run.progress.done += cmd.len() as u64 + 1;
                        return Some((cmd.to_string(), Kind::Upload));
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
                    self.status.activity = activity(&cmd).or(self.status.activity);
                    self.send_numbered(&cmd, Kind::Manual, events);
                }
                Queued::Kinded(cmd, kind) => self.send_numbered(&cmd, kind, events),
                Queued::Resync => {
                    let n = self.next_n;
                    self.send_numbered(&format!("M110 N{n}"), Kind::Setup, events);
                }
            }
            return;
        }
        if self.job.as_ref().is_some_and(Running::uploading) {
            if let Some((cmd, kind)) = self.next_upload_line() {
                self.send_numbered(&cmd, kind, events);
                if let (Some(run), Some(p)) = (&self.job, self.status.progress.as_mut()) {
                    p.done = run.progress.done;
                }
            }
            return;
        }
        if let Some(cmd) = self.polls.pop_front() {
            self.send_numbered(cmd, Kind::Poll, events);
            return;
        }
        let quiet = self.last_rx.elapsed() > QUIET_POLL;
        if (!self.autoreport && self.last_poll.elapsed() > POLL_EVERY) || quiet {
            self.last_poll = Instant::now();
            self.send_numbered("M105", Kind::Poll, events);
            return;
        }
        let every = if self.job.is_some() || self.status.foreign.is_some() {
            SD_POLL_BUSY
        } else {
            SD_POLL_IDLE
        };
        let idle = self.job.is_none() && self.status.foreign.is_none();
        if self.sd_cap != Some(false) && idle && self.last_card_poll.elapsed() > CARD_POLL {
            self.last_card_poll = Instant::now();
            self.polls.push_back("M20");
        }
        if self.sd_cap != Some(false) && self.last_sd_poll.elapsed() > every {
            self.last_sd_poll = Instant::now();
            self.polls.extend(["M27 C", "M27"]);
        }
    }

    fn expire_heating(&mut self) {
        if self.status.activity == Some("heating") {
            if let Some(at) = self.last_heating
                && at.elapsed() > HEAT_EXPIRY
            {
                self.status.activity = None;
                self.last_heating = None;
            }
        }
    }

    fn probe(&mut self, events: &mut Vec<Event>) {
        let Handshake::Probing { last, tries } = self.handshake else { return };
        let due = match last {
            None => self.attached_at.elapsed() > Duration::from_millis(500),
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

    fn stop_upload(&mut self) -> Option<Running> {
        let run = self.job.take()?;
        self.resend.clear();
        self.resend_from = None;
        self.manual.clear();
        self.manual.push_back(Queued::Resync);
        if !run.card {
            self.manual.push_back(Queued::Plain("M29".into()));
            self.manual.push_back(Queued::Plain(format!("M30 {}", run.file)));
        }
        self.status.progress = None;
        self.status.state = Some(State::Idle);
        Some(run)
    }

    fn cancel_with(&mut self, events: &mut Vec<Event>, reason: &str) -> Result<(), String> {
        match &self.job {
            None if self.status.foreign.is_some() => {
                self.manual.push_back(Queued::Plain("M524".into()));
                return Ok(());
            }
            None => return Err("nothing is printing".into()),
            Some(run) if run.uploading() => {
                if let Some(run) = self.stop_upload() {
                    events.push(Event::Interrupted(
                        run.progress.job,
                        "cancelled while it was being sent".into(),
                    ));
                }
                return Ok(());
            }
            Some(_) => {}
        }
        if self.write("M108", events) {
            events.push(Event::Console(Dir::Sent, "M108".into()));
        }
        for cmd in ["M524", "M104 S0", "M140 S0", "M107"] {
            self.manual.push_back(Queued::Plain(cmd.into()));
        }
        self.end_job(Outcome::Cancelled, reason, events);
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
        let uploading = self.job.as_ref().is_some_and(Running::uploading);
        if self.in_flight.is_some()
            && self.last_rx.elapsed() > POKE_AFTER
            && !self.poked
            && !uploading
        {
            self.poked = true;
            events.push(Event::Console(Dir::Note, "no answer for 15 s, asking again".into()));
            self.write("M105", events);
        }
        self.pump(events);
        self.expire_heating();
    }

    fn start(&mut self, job: Job) -> Result<(), String> {
        if self.status.state() != State::Idle || self.job.is_some() {
            return Err(format!("the printer is {}", self.status.state().label()));
        }
        if self.sd_cap == Some(false) {
            return Err("this printer has no SD card to print from".into());
        }
        if let Some(name) = job.card.clone() {
            let (layers, bytes) = gcode::upload_layers(&job.path).unwrap_or_default();
            let progress = Progress {
                job,
                done: 0,
                total: 0,
                layer: 0,
                started: SystemTime::now(),
                paused: Duration::ZERO,
                paused_since: None,
                sending: false,
            };
            self.status.progress = Some(progress.clone());
            self.status.message = None;
            self.manual.push_back(Queued::Plain("M21".into()));
            self.manual.push_back(Queued::Kinded(format!("M23 {name}"), Kind::Open));
            self.job = Some(Running {
                progress,
                file: name,
                phase: Phase::Selecting,
                layers,
                bytes,
                card: true,
            });
            self.status.state = Some(State::Printing);
            return Ok(());
        }
        let (layers, total) =
            gcode::upload_layers(&job.path).map_err(|e| format!("{}: {e}", job.path.display()))?;
        let file = File::open(&job.path).map_err(|e| format!("{}: {e}", job.path.display()))?;
        let name = sd_name(job.id);
        let progress = Progress {
            job,
            done: 0,
            total,
            layer: 0,
            started: SystemTime::now(),
            paused: Duration::ZERO,
            paused_since: None,
            sending: true,
        };
        self.status.progress = Some(progress.clone());
        self.status.message = None;
        self.manual.push_back(Queued::Plain("M21".into()));
        self.manual.push_back(Queued::Kinded(format!("M28 {name}"), Kind::Open));
        self.job = Some(Running {
            progress,
            file: name,
            phase: Phase::Opening(BufReader::with_capacity(1 << 16, file)),
            layers,
            bytes: total,
            card: false,
        });
        self.status.state = Some(State::Printing);
        Ok(())
    }

    fn pause(&mut self) -> Result<(), String> {
        if self.status.state() != State::Printing
            || self.job.as_ref().is_none_or(Running::uploading)
        {
            return Err("nothing is printing on the printer".into());
        }
        self.manual.push_back(Queued::Plain("M25".into()));
        self.ours(None);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        if self.status.state() != State::Paused {
            return Err("the printer is not paused".into());
        }
        self.manual.push_back(Queued::Plain("M24".into()));
        if self.job.is_some() {
            self.ours(Some((0, 0)));
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
        if self.job.as_ref().is_some_and(Running::uploading) {
            return Err("the printer is being sent a file; wait for it to finish".into());
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
        if self.job.as_ref().is_some_and(Running::uploading) {
            self.interrupt("reconnected while it was being sent", &mut events);
        }
        self.pending.extend(events);
        self.away = self.job.is_some();
        self.link = None;
        let progress = self.status.progress.take();
        self.status = Status { progress, ..Status::default() };
        self.retry_at = Instant::now();
    }

    fn adopt(&mut self, job: Job, started: SystemTime, on_printer: bool) {
        let file = sd_name(job.id);
        let card = job.card.is_some();
        let file = job.card.clone().unwrap_or(file);
        if !on_printer {
            if !card {
                self.cleanup.push(file);
            }
            self.pending
                .push(Event::Interrupted(job, "printfarm closed while it was being sent".into()));
            return;
        }
        let (layers, bytes) = gcode::upload_layers(&job.path).unwrap_or_default();
        let progress = Progress {
            job,
            done: 0,
            total: 0,
            layer: 0,
            started,
            paused: Duration::ZERO,
            paused_since: None,
            sending: false,
        };
        self.status.progress = Some(progress.clone());
        self.job = Some(Running { progress, file, phase: Phase::OnPrinter, layers, bytes, card });
        self.away = true;
    }

    fn leave(&mut self) {
        if self.job.as_ref().is_some_and(Running::uploading) {
            self.stop_upload();
        }
    }
}

pub fn open(path: &str, baud: u32) -> Result<Box<dyn SerialPort>, String> {
    serialport::new(path, baud).timeout(READ_TIMEOUT).open().map_err(|e| {
        let there = std::path::Path::new(path).exists();
        match e.kind() {
            serialport::ErrorKind::NoDevice | serialport::ErrorKind::Io(_) if !there => {
                format!("{path} is not there. Plug the printer in and turn it on.")
            }
            serialport::ErrorKind::NoDevice => {
                format!("{path} is busy. Another program has the printer open.")
            }
            _ => format!("cannot open {path}: {e}"),
        }
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

fn card_file(line: &str) -> Option<CardFile> {
    let mut words = line.split_whitespace();
    let name = words.next()?;
    let size = words.next()?.parse().ok()?;
    (words.next().is_none() && !name.ends_with('/'))
        .then(|| CardFile { name: name.to_string(), size })
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

    fn with_job() -> Marlin {
        let mut m = Marlin::new(String::new(), 0);
        m.job = Some(Running {
            progress: Progress {
                job: Job {
                    id: 1,
                    name: "order".into(),
                    path: std::path::PathBuf::from("order.gcode"),
                    printer: None,
                    added: 0,
                    meta: Default::default(),
                    card: None,
                },
                done: 10,
                total: 100,
                layer: 1,
                started: SystemTime::now(),
                paused: Duration::ZERO,
                paused_since: None,
                sending: false,
            },
            file: "PF000001.GCO".into(),
            phase: Phase::OnPrinter,
            layers: Vec::new(),
            bytes: 100,
            card: false,
        });
        m
    }

    #[test]
    fn a_heating_marker_survives_the_plain_reports_that_interleave() {
        let mut m = with_job();
        let mut ev = Vec::new();
        m.line(" T:150.00 /210.00 B:35.00 /60.00 @:255 B@:255 W:20", &mut ev);
        assert_eq!(m.status.activity, Some("heating"));
        for _ in 0..5 {
            m.line(" T:150.00 /210.00 B:35.00 /60.00 @:255 B@:255", &mut ev);
            assert_eq!(m.status.activity, Some("heating"));
        }
        m.line(" T:160.00 /210.00 B:40.00 /60.00 @:255 B@:255 W:14", &mut ev);
        assert_eq!(m.status.activity, Some("heating"));
    }

    #[test]
    fn the_heating_label_expires_when_the_marker_stops() {
        let mut m = with_job();
        let mut ev = Vec::new();
        m.line(" T:150.00 /210.00 B:35.00 /60.00 @:255 B@:255 W:20", &mut ev);
        assert_eq!(m.status.activity, Some("heating"));
        m.last_heating = Some(Instant::now() - HEAT_EXPIRY - Duration::from_secs(1));
        m.expire_heating();
        assert_eq!(m.status.activity, None);
        m.line(" T:150.00 /210.00 B:35.00 /60.00 @:255 B@:255 W:19", &mut ev);
        m.expire_heating();
        assert_eq!(m.status.activity, Some("heating"));
    }

    #[test]
    fn a_heating_marker_does_not_hide_a_specific_activity() {
        let mut m = with_job();
        let mut ev = Vec::new();
        m.status.activity = Some("heating the bed");
        m.line(" T:150.00 /210.00 B:35.00 /60.00 @:255 B@:255 W:20", &mut ev);
        assert_eq!(m.status.activity, Some("heating the bed"));
    }
}
