use crate::gcode::checksum;
use serialport::{SerialPort, TTYPort};
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
}

#[derive(Default)]
pub struct Seen {
    pub commands: Vec<String>,
    pub resends: usize,
}

pub struct Sim {
    pub seen: Arc<Mutex<Seen>>,
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
}

fn param(cmd: &str, key: char) -> Option<f32> {
    cmd.split_whitespace().skip(1).find_map(|w| w.strip_prefix(key)?.parse().ok())
}

pub fn spawn(mut port: TTYPort, mut script: Script) -> Sim {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (s, st) = (seen.clone(), stop.clone());
    let _ = port.set_timeout(Duration::from_millis(10));
    std::thread::spawn(move || {
        let mut last: u32 = 0;
        let mut rx = Vec::new();
        let mut buf = [0u8; 256];
        let mut autoreport = None::<Instant>;
        let mut halted = false;
        let mut heat = Heaters { hotend: (21.0, 0.0), bed: (21.0, 0.0), at: Instant::now() };
        let say = |port: &mut TTYPort, text: &str| {
            let _ = port.write_all(format!("{text}\n").as_bytes());
        };
        while !st.load(Ordering::Relaxed) {
            match port.read(&mut buf) {
                Ok(n) => rx.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
            heat.tick(script.heat_rate);
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
                let word = cmd.split_whitespace().next().unwrap_or("");
                match word {
                    "M104" | "M109" => heat.hotend.1 = param(&cmd, 'S').unwrap_or(0.0),
                    "M140" | "M190" => heat.bed.1 = param(&cmd, 'S').unwrap_or(0.0),
                    _ => {}
                }
                if word == "M109" || word == "M190" {
                    let wait = Instant::now();
                    loop {
                        if let Ok(n) = port.read(&mut buf) {
                            rx.extend_from_slice(&buf[..n]);
                        }
                        if rx.windows(5).any(|w| w == b"M108\n") {
                            break;
                        }
                        heat.tick(script.heat_rate);
                        let (a, t) = if word == "M109" { heat.hotend } else { heat.bed };
                        if (a - t).abs() < 1.0
                            || wait.elapsed() > Duration::from_secs(120)
                            || st.load(Ordering::Relaxed)
                        {
                            break;
                        }
                        say(&mut port, &format!("{} W:?", heat.report()));
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
                if word.starts_with("G1") || word.starts_with("G0") {
                    std::thread::sleep(script.move_delay);
                }
                match cmd.as_str() {
                    "M115" => {
                        say(
                            &mut port,
                            "FIRMWARE_NAME:Marlin 2.1.2 (simulated) SOURCE_CODE_URL:github.com/MarlinFirmware/Marlin PROTOCOL_VERSION:1.0 MACHINE_TYPE:Simulated Printer EXTRUDER_COUNT:1",
                        );
                        say(&mut port, "Cap:AUTOREPORT_TEMP:1");
                        say(&mut port, "ok");
                    }
                    "M105" => say(&mut port, &format!("ok {}", heat.report())),
                    "M155 S2" => {
                        autoreport = Some(Instant::now());
                        say(&mut port, "ok");
                    }
                    _ => say(&mut port, "ok"),
                }
            }
        }
    });
    Sim { seen, stop }
}
