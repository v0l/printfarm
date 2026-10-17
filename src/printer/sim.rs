use crate::gcode::checksum;
use serialport::{SerialPort, TTYPort};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default, Clone)]
pub struct Script {
    pub corrupt_once: Option<u32>,
    pub halt_at: Option<String>,
    pub busy_on: Option<String>,
    pub move_delay: Duration,
    pub heat_rate: f32,
    pub sd_lines_per_s: f32,
    pub no_card: bool,
    pub files: Vec<(String, String)>,
    pub printing: Option<String>,
}

#[derive(Default)]
pub struct Seen {
    pub commands: Vec<String>,
    pub resends: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Open {
    pub name: String,
    pub pos: usize,
    pub printing: bool,
}

#[derive(Default)]
pub struct Card {
    pub files: BTreeMap<String, Vec<u8>>,
    pub open: Option<Open>,
    pub finished: Vec<String>,
}

pub struct Sim {
    pub seen: Arc<Mutex<Seen>>,
    pub card: Arc<Mutex<Card>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Sim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Heaters {
    hotend: (f32, f32),
    bed: (f32, f32),
    at: Instant,
}

impl Heaters {
    fn tick(&mut self, rate: f32) {
        let dt = self.at.elapsed().as_secs_f32();
        self.at = Instant::now();
        for (actual, target) in [&mut self.hotend, &mut self.bed] {
            let goal = target.max(21.0);
            let step = if rate > 0.0 { rate * dt } else { f32::MAX };
            *actual += (goal - *actual).clamp(-step, step);
        }
    }

    fn report(&self) -> String {
        format!(
            "T:{:.2} /{:.2} B:{:.2} /{:.2} @:0 B@:0",
            self.hotend.0, self.hotend.1, self.bed.0, self.bed.1
        )
    }

    fn set(&mut self, cmd: &str) {
        let word = cmd.split_whitespace().next().unwrap_or("");
        match word {
            "M104" | "M109" => self.hotend.1 = param(cmd, 'S').unwrap_or(0.0),
            "M140" | "M190" => self.bed.1 = param(cmd, 'S').unwrap_or(0.0),
            _ => {}
        }
    }
}

fn param(cmd: &str, key: char) -> Option<f32> {
    cmd.split_whitespace().skip(1).find_map(|w| w.strip_prefix(key)?.parse().ok())
}

fn say(port: &mut TTYPort, text: &str) {
    let _ = port.write_all(format!("{text}\n").as_bytes());
}

fn wait_for_heat(
    port: &mut TTYPort,
    heat: &mut Heaters,
    rate: f32,
    rx: &mut Vec<u8>,
    autoreport: &mut Option<Instant>,
    stop: &AtomicBool,
    bed: bool,
) {
    let wait = Instant::now();
    let mut buf = [0u8; 256];
    loop {
        if let Ok(n) = port.read(&mut buf) {
            rx.extend_from_slice(&buf[..n]);
        }
        if rx.windows(5).any(|w| w == b"M108\n") {
            break;
        }
        heat.tick(rate);
        let (a, t) = if bed { heat.bed } else { heat.hotend };
        if (a - t).abs() < 1.0
            || wait.elapsed() > Duration::from_secs(120)
            || stop.load(Ordering::Relaxed)
        {
            break;
        }
        say(port, &format!("{} W:?", heat.report()));
        if let Some(at) = *autoreport
            && at.elapsed() > Duration::from_millis(300)
        {
            say(port, &format!(" {}", heat.report()));
            *autoreport = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn arg(cmd: &str) -> String {
    cmd.split_whitespace().nth(1).unwrap_or("").to_ascii_uppercase()
}

const MESH: [[f32; 5]; 5] = [
    [0.485, 0.295, 0.182, 0.095, 0.110],
    [0.388, 0.230, 0.108, -0.020, -0.068],
    [0.375, 0.210, 0.040, -0.102, -0.152],
    [0.380, 0.180, 0.010, -0.148, -0.237],
    [0.418, 0.158, -0.023, -0.183, -0.287],
];

pub fn spawn(mut port: TTYPort, mut script: Script) -> Sim {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let card = Arc::new(Mutex::new(Card::default()));
    {
        let mut c = card.lock().unwrap();
        for (name, body) in &script.files {
            c.files.insert(name.to_ascii_uppercase(), body.clone().into_bytes());
        }
        if let Some(name) = &script.printing {
            c.open = Some(Open { name: name.to_ascii_uppercase(), pos: 0, printing: true });
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (s, st, sd) = (seen.clone(), stop.clone(), card.clone());
    let _ = port.set_timeout(Duration::from_millis(10));
    std::thread::spawn(move || {
        let mut last: u32 = 0;
        let mut rx = Vec::new();
        let mut buf = [0u8; 256];
        let mut autoreport = None::<Instant>;
        let mut halted = false;
        let mut saving: Option<(String, Vec<u8>)> = None;
        let mut leveling = false;
        let mut probed = false;
        let mut sd_clock = Instant::now();
        let mut sd_due = 0.0f32;
        let mut heat = Heaters { hotend: (21.0, 0.0), bed: (21.0, 0.0), at: Instant::now() };
        let say = |port: &mut TTYPort, text: &str| {
            let _ = port.write_all(format!("{text}\n").as_bytes());
        };
        let grid = |port: &mut TTYPort| {
            say(port, "Bilinear Leveling Grid:");
            say(port, "      0      1      2      3      4");
            for (y, row) in MESH.iter().enumerate() {
                let cells: Vec<String> = row.iter().map(|v| format!("{v:+.3}")).collect();
                say(port, &format!(" {y} {}", cells.join(" ")));
            }
        };
        while !st.load(Ordering::Relaxed) {
            match port.read(&mut buf) {
                Ok(n) => rx.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
            heat.tick(script.heat_rate);
            let rate = if script.sd_lines_per_s > 0.0 { script.sd_lines_per_s } else { 2000.0 };
            sd_due += sd_clock.elapsed().as_secs_f32() * rate;
            sd_clock = Instant::now();
            {
                let mut c = sd.lock().unwrap();
                let mut done = None;
                if !c.open.as_ref().is_some_and(|o| o.printing) {
                    sd_due = 0.0;
                }
                if let Some(open) = c.open.clone().filter(|o| o.printing) {
                    let data = c.files.get(&open.name).cloned().unwrap_or_default();
                    let mut pos = open.pos;
                    while sd_due >= 1.0 && pos < data.len() {
                        let end = data[pos..]
                            .iter()
                            .position(|b| *b == b'\n')
                            .map_or(data.len(), |i| pos + i + 1);
                        let line = String::from_utf8_lossy(&data[pos..end]).trim().to_string();
                        heat.set(&line);
                        pos = end;
                        sd_due -= 1.0;
                        let word = line.split_whitespace().next().unwrap_or("");
                        if word == "M109" || word == "M190" {
                            drop(c);
                            wait_for_heat(
                                &mut port,
                                &mut heat,
                                script.heat_rate,
                                &mut rx,
                                &mut autoreport,
                                &st,
                                word == "M190",
                            );
                            c = sd.lock().unwrap();
                        }
                    }
                    if pos >= data.len() {
                        done = Some(open.name.clone());
                    } else if let Some(o) = c.open.as_mut() {
                        o.pos = pos;
                    }
                }
                if let Some(name) = done {
                    c.open = None;
                    c.finished.push(name);
                    say(&mut port, "Done printing file");
                }
                sd_due = sd_due.min(rate);
            }
            if let Some(at) = autoreport
                && at.elapsed() > Duration::from_millis(300)
            {
                say(&mut port, &format!(" {}", heat.report()));
                autoreport = Some(Instant::now());
            }
            while let Some(i) = rx.iter().position(|b| *b == b'\n') {
                let raw: Vec<u8> = rx.drain(..=i).collect();
                let line = String::from_utf8_lossy(&raw).trim().to_string();
                if line.is_empty() || halted {
                    continue;
                }
                let cmd = match line.strip_prefix('N') {
                    Some(rest) => {
                        let Some((body, cs)) = line.rsplit_once('*') else { continue };
                        let Some((n, cmd)) = rest.split_once(' ') else { continue };
                        let Ok(n) = n.parse::<u32>() else { continue };
                        let cmd = cmd.rsplit_once('*').map(|(c, _)| c).unwrap_or(cmd).to_string();
                        let bad_sum = cs.parse::<u8>().ok() != Some(checksum(body));
                        if bad_sum || script.corrupt_once == Some(n) {
                            script.corrupt_once = None;
                            s.lock().unwrap().resends += 1;
                            say(&mut port, &format!("Error:checksum mismatch, Last Line: {last}"));
                            say(&mut port, &format!("Resend: {}", last + 1));
                            say(&mut port, "ok");
                            continue;
                        }
                        if let Some(to) = cmd.strip_prefix("M110 N") {
                            last = to.parse().unwrap_or(0);
                        } else if n != last + 1 {
                            s.lock().unwrap().resends += 1;
                            say(
                                &mut port,
                                &format!(
                                    "Error:Line Number is not Last Line Number+1, Last Line: {last}"
                                ),
                            );
                            say(&mut port, &format!("Resend: {}", last + 1));
                            say(&mut port, "ok");
                            continue;
                        } else {
                            last = n;
                        }
                        cmd
                    }
                    None => line.clone(),
                };
                s.lock().unwrap().commands.push(cmd.clone());
                let word = cmd.split_whitespace().next().unwrap_or("").to_string();
                if let Some((name, body)) = saving.as_mut() {
                    if word == "M29" {
                        let name = name.clone();
                        let body = std::mem::take(body);
                        sd.lock().unwrap().files.insert(name, body);
                        saving = None;
                        say(&mut port, "Done saving file.");
                    } else {
                        body.extend_from_slice(cmd.as_bytes());
                        body.push(b'\n');
                    }
                    say(&mut port, "ok");
                    continue;
                }
                if script.halt_at.as_deref() == Some(cmd.as_str()) {
                    say(&mut port, "Error:Printer halted. kill() called!");
                    halted = true;
                    continue;
                }
                if script.busy_on.as_deref() == Some(cmd.as_str()) {
                    for _ in 0..3 {
                        say(&mut port, "echo:busy: processing");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                heat.set(&cmd);
                if word == "M109" || word == "M190" {
                    wait_for_heat(
                        &mut port,
                        &mut heat,
                        script.heat_rate,
                        &mut rx,
                        &mut autoreport,
                        &st,
                        word == "M190",
                    );
                }
                if word.starts_with("G1") || word.starts_with("G0") {
                    std::thread::sleep(script.move_delay);
                }
                let mut c = sd.lock().unwrap();
                match (word.as_str(), cmd.as_str()) {
                    ("M115", _) => {
                        say(
                            &mut port,
                            "FIRMWARE_NAME:Marlin 2.1.2 (simulated) SOURCE_CODE_URL:github.com/MarlinFirmware/Marlin PROTOCOL_VERSION:1.0 MACHINE_TYPE:Simulated Printer EXTRUDER_COUNT:1",
                        );
                        say(&mut port, "Cap:AUTOREPORT_TEMP:1");
                        say(&mut port, "Cap:SDCARD:1");
                    }
                    ("M105", _) => {
                        say(&mut port, &format!("ok {}", heat.report()));
                        continue;
                    }
                    ("M155", _) => autoreport = Some(Instant::now()),
                    ("M20", _) if script.no_card => say(&mut port, "echo:No SD card"),
                    ("M20", _) => {
                        say(&mut port, "Begin file list");
                        let mut names: Vec<_> = c.files.iter().collect();
                        names.sort();
                        for (name, body) in names {
                            say(&mut port, &format!("{name} {}", body.len()));
                        }
                        say(&mut port, "End file list");
                    }
                    ("M21", _) if script.no_card => say(&mut port, "echo:No SD card"),
                    ("M21", _) => say(&mut port, "echo:SD card ok"),
                    ("M28", _) if script.no_card => {
                        say(&mut port, &format!("open failed, File: {}.", arg(&cmd)))
                    }
                    ("M28", _) => {
                        say(
                            &mut port,
                            &format!("Writing to file: {}", arg(&cmd).to_ascii_lowercase()),
                        );
                        saving = Some((arg(&cmd), Vec::new()));
                    }
                    ("M23", _) => {
                        let name = arg(&cmd);
                        match c.files.get(&name) {
                            Some(body) => {
                                say(
                                    &mut port,
                                    &format!(
                                        "File opened: {} Size: {}",
                                        name.to_ascii_lowercase(),
                                        body.len()
                                    ),
                                );
                                say(&mut port, "File selected");
                                c.open = Some(Open { name, pos: 0, printing: false });
                            }
                            None => say(
                                &mut port,
                                &format!("open failed, File: {}.", name.to_ascii_lowercase()),
                            ),
                        }
                    }
                    ("M24", _) => {
                        if let Some(o) = c.open.as_mut() {
                            o.printing = true;
                        }
                    }
                    ("M25", _) => {
                        if let Some(o) = c.open.as_mut() {
                            o.printing = false;
                        }
                    }
                    ("M524", _) => {
                        c.open = None;
                        heat.hotend.1 = 0.0;
                        heat.bed.1 = 0.0;
                    }
                    (_, "M27 C") => match &c.open {
                        Some(o) => say(
                            &mut port,
                            &format!("Current file: {} {}", o.name.to_ascii_lowercase(), o.name),
                        ),
                        None => say(&mut port, "Current file: (no file)"),
                    },
                    ("M27", _) => match c.open.clone().filter(|o| o.printing) {
                        Some(o) => {
                            let size = c.files.get(&o.name).map_or(0, Vec::len);
                            say(&mut port, &format!("SD printing byte {}/{size}", o.pos));
                        }
                        None => say(&mut port, "Not SD printing"),
                    },
                    ("M30", _) => {
                        let name = arg(&cmd);
                        if c.files.remove(&name).is_some() {
                            say(&mut port, &format!("File deleted:{}", name.to_ascii_lowercase()));
                        } else {
                            say(&mut port, "Deletion failed");
                        }
                    }
                    ("M420", _) => {
                        if let Some(s) = param(&cmd, 'S') {
                            leveling = s > 0.5 && probed;
                        }
                        if cmd.contains('V') && probed {
                            grid(&mut port);
                        }
                        say(
                            &mut port,
                            &format!("echo:Bed Leveling {}", if leveling { "ON" } else { "OFF" }),
                        );
                        say(&mut port, "echo:Fade Height 10.00");
                    }
                    ("G29", _) => {
                        for _ in 0..3 {
                            say(&mut port, "echo:busy: processing");
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        probed = true;
                        leveling = true;
                        grid(&mut port);
                    }
                    _ => {}
                }
                say(&mut port, "ok");
            }
        }
    });
    Sim { seen, card, stop }
}
