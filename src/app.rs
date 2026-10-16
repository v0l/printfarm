use crate::camera::Camera;
use crate::config::{self, Config, Link, PrinterConfig};
use crate::farm::{self, Queue, Record};
use crate::gcode;
use crate::printer::marlin::{self, BAUDS, Detected, PortInfo};
use crate::printer::{Dir, Event, Outcome, State};
use crate::station::{Control, Station, Wake};
use egui::{Color32, FontId, Id, Rect, Sense, Stroke, TextureHandle, TextureOptions, Ui, Vec2};
use egui_bench::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CARD_W: f32 = 380.0;
const PRESETS: [(&str, u32, u32); 3] = [("PLA", 210, 60), ("PETG", 240, 80), ("ABS", 250, 100)];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Printers,
    Queue,
    History,
}

enum Act {
    Start(usize, u64),
    Pause(usize),
    Resume(usize),
    Cancel(usize),
    Gcode(usize, String),
    Reconnect(usize),
    BedClear(usize),
    Edit(Option<usize>),
    Remove(usize),
    Console(usize),
    Camera(usize),
    Pick(Option<String>),
    Shift(u64, isize),
    Drop(u64),
    Assign(u64, Option<String>),
    Again(usize),
    Mesh(usize),
    Probe(usize),
    SaveMesh(usize),
}

struct Editor {
    index: Option<usize>,
    name: String,
    port: String,
    baud: u32,
    camera: String,
    ports: Vec<PortInfo>,
    detecting: Option<Receiver<Result<Detected, String>>>,
    found: Option<(bool, String)>,
}

impl Editor {
    fn new(index: Option<usize>, existing: Option<&PrinterConfig>) -> Editor {
        let ports = marlin::ports();
        let (port, baud) = match existing.map(|p| &p.link) {
            Some(Link::MarlinUsb { port, baud }) => (port.clone(), *baud),
            None => (ports.first().map(|p| p.path.clone()).unwrap_or_default(), 115200),
        };
        Editor {
            index,
            name: existing.map(|p| p.name.clone()).unwrap_or_default(),
            port,
            baud,
            camera: existing.and_then(|p| p.camera.clone()).unwrap_or_default(),
            ports,
            detecting: None,
            found: None,
        }
    }
}

struct Thumb {
    texture: Option<TextureHandle>,
}

pub struct App {
    config: Config,
    config_path: PathBuf,
    stations: Vec<Station>,
    cameras: HashMap<String, Camera>,
    frames: HashMap<String, (u64, TextureHandle)>,
    thumbs: HashMap<PathBuf, Thumb>,
    queue: Queue,
    queue_path: PathBuf,
    history: Vec<Record>,
    history_path: PathBuf,
    events: Receiver<(String, Event)>,
    events_tx: Sender<(String, Event)>,
    wake: Wake,
    tab: Tab,
    editor: Option<Editor>,
    browser: Option<(Browser, Option<String>)>,
    console: Option<String>,
    console_line: String,
    removing: Option<usize>,
    cancelling: Option<usize>,
    closing: bool,
    big_camera: Option<String>,
    mesh: Option<String>,
    probing: Option<(usize, u32)>,
    saving_mesh: Option<usize>,
    notice: Option<(bool, String)>,
}

impl App {
    pub fn new(ctx: &egui::Context, open: Vec<PathBuf>) -> App {
        egui_bench::install(ctx);
        let c = ctx.clone();
        let wake: Wake = Arc::new(move || c.request_repaint());
        let (events_tx, events) = channel();
        let config_path = config::config_path();
        let queue_path = farm::queue_path();
        let history_path = farm::history_path();
        let mut notice = None;
        let config = Config::load(&config_path).unwrap_or_else(|e| {
            notice = Some((false, e));
            Config::default()
        });
        let queue = Queue::load(&queue_path).unwrap_or_else(|e| {
            notice = Some((false, e));
            Queue::default()
        });
        let mut app = App {
            config,
            config_path,
            stations: Vec::new(),
            cameras: HashMap::new(),
            frames: HashMap::new(),
            thumbs: HashMap::new(),
            queue,
            queue_path,
            history: farm::history(&history_path),
            history_path,
            events,
            events_tx,
            wake,
            tab: Tab::Printers,
            editor: None,
            browser: None,
            console: None,
            console_line: String::new(),
            removing: None,
            cancelling: None,
            closing: false,
            big_camera: None,
            mesh: None,
            probing: None,
            saving_mesh: None,
            notice,
        };
        app.restart_stations();
        for path in open {
            app.enqueue(&path, None);
        }
        app
    }

    fn printer_name(&self, id: &str) -> String {
        self.config
            .printers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    fn restart_stations(&mut self) {
        self.stations.clear();
        for i in 0..self.config.printers.len() {
            let station = self.station(self.config.printers[i].clone());
            self.stations.push(station);
        }
        self.sync_cameras();
    }

    fn station(&self, p: PrinterConfig) -> Station {
        let watch =
            self.queue.running.get(&p.id).map(|r| {
                (r.job.clone(), UNIX_EPOCH + Duration::from_secs(r.started), r.on_printer)
            });
        Station::spawn(p, watch, self.events_tx.clone(), self.wake.clone())
    }

    fn sync_cameras(&mut self) {
        let want: HashMap<String, String> = self
            .config
            .printers
            .iter()
            .filter_map(|p| {
                p.camera.clone().filter(|u| !u.trim().is_empty()).map(|u| (p.id.clone(), u))
            })
            .collect();
        self.cameras.retain(|id, cam| want.get(id) == Some(&cam.url));
        self.frames.retain(|id, _| want.contains_key(id));
        for (id, url) in want {
            if !self.cameras.contains_key(&id) {
                self.cameras.insert(id, Camera::start(url, self.wake.clone()));
            }
        }
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.config_path) {
            self.notice = Some((false, e));
        }
    }

    fn save_queue(&mut self) {
        if let Err(e) = self.queue.save(&self.queue_path) {
            self.notice = Some((false, e));
        }
    }

    fn log(&mut self, record: Record) {
        if let Err(e) = farm::append(&self.history_path, &record) {
            self.notice = Some((false, e));
        }
        self.history.push(record);
    }

    fn enqueue(&mut self, path: &Path, printer: Option<String>) {
        match self.queue.add(path, printer) {
            Ok(id) => {
                let name = self.queue.find(id).map(|j| j.name.clone()).unwrap_or_default();
                self.notice = Some((true, format!("{name} is queued")));
                self.save_queue();
            }
            Err(e) => self.notice = Some((false, e)),
        }
    }

    fn events(&mut self) {
        while let Ok((printer, e)) = self.events.try_recv() {
            let name = self.printer_name(&printer);
            match e {
                Event::Started(job) => {
                    let run = farm::Running { job, started: farm::now(), on_printer: false };
                    self.queue.running.insert(printer, run);
                    self.save_queue();
                }
                Event::OnPrinter(job) => {
                    let run = farm::Running { job, started: farm::now(), on_printer: true };
                    self.queue.running.insert(printer, run);
                    self.save_queue();
                }
                Event::Interrupted(job, why) => {
                    self.notice = Some((
                        false,
                        format!("{} is back at the top of the queue: {why}", job.name),
                    ));
                    self.queue.running.remove(&printer);
                    self.queue.jobs.insert(0, job);
                    self.save_queue();
                }
                Event::Refused(job, why) => {
                    self.notice =
                        Some((false, format!("{name} did not start {}: {why}", job.name)));
                    self.queue.jobs.insert(0, job);
                    self.save_queue();
                }
                Event::Finished { job, outcome, started, reason } => {
                    let said = match outcome {
                        Outcome::Done => format!("{} finished on {name}", job.name),
                        Outcome::Cancelled => format!("{} was cancelled on {name}", job.name),
                        Outcome::Failed => format!("{} failed on {name}: {reason}", job.name),
                    };
                    self.notice = Some((outcome == Outcome::Done, said));
                    self.log(Record::new(&job, &name, farm::unix(started), outcome, &reason));
                    self.queue.running.remove(&printer);
                    self.queue.unclear.insert(printer);
                    self.save_queue();
                }
                Event::Console(..) => {}
            }
        }
    }

    fn apply(&mut self, acts: Vec<Act>) {
        for act in acts {
            match act {
                Act::Start(i, id) => {
                    if let Some(job) = self.queue.take(id) {
                        self.stations[i].send(Control::Start(job));
                        self.save_queue();
                    }
                }
                Act::Pause(i) => self.stations[i].send(Control::Pause),
                Act::Resume(i) => self.stations[i].send(Control::Resume),
                Act::Cancel(i) => self.cancelling = Some(i),
                Act::Gcode(i, g) => self.stations[i].send(Control::Gcode(g)),
                Act::Reconnect(i) => self.stations[i].send(Control::Reconnect),
                Act::BedClear(i) => {
                    self.queue.unclear.remove(&self.config.printers[i].id);
                    self.save_queue();
                }
                Act::Edit(i) => {
                    self.editor = Some(Editor::new(i, i.map(|i| &self.config.printers[i])))
                }
                Act::Remove(i) => self.removing = Some(i),
                Act::Console(i) => self.console = Some(self.config.printers[i].id.clone()),
                Act::Mesh(i) => self.mesh = Some(self.config.printers[i].id.clone()),
                Act::Probe(i) => self.probing = Some((i, 0)),
                Act::SaveMesh(i) => self.saving_mesh = Some(i),
                Act::Camera(i) => self.big_camera = Some(self.config.printers[i].id.clone()),
                Act::Pick(printer) => {
                    let dir = self
                        .queue
                        .jobs
                        .last()
                        .and_then(|j| j.path.parent().map(Path::to_path_buf))
                        .or_else(dirs::home_dir)
                        .unwrap_or_default();
                    self.browser = Some((
                        Browser::new("queue a G-code file", dir).suffixes(&gcode::SUFFIXES),
                        printer,
                    ));
                }
                Act::Shift(id, by) => {
                    self.queue.shift(id, by);
                    self.save_queue();
                }
                Act::Drop(id) => {
                    self.queue.take(id);
                    self.save_queue();
                }
                Act::Assign(id, p) => {
                    self.queue.assign(id, p);
                    self.save_queue();
                }
                Act::Again(i) => {
                    let path = self.history[i].path.clone();
                    let printer = self
                        .config
                        .printers
                        .iter()
                        .find(|p| p.name == self.history[i].printer)
                        .map(|p| p.id.clone());
                    self.enqueue(&path, printer);
                }
            }
        }
    }

    fn sending(&self) -> Vec<String> {
        self.stations
            .iter()
            .filter_map(|s| s.status().progress.filter(|p| p.sending).map(|p| p.job.name))
            .collect()
    }

    fn busy(&self) -> bool {
        self.stations.iter().any(|s| s.status().state().busy())
    }

    fn header(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("PRINTFARM").font(legend_font(17.0)).color(VALUE));
            ui.add_space(18.0);
            let queued = self.queue.jobs.len();
            let opts = [
                (Tab::Printers, format!("printers {}", self.stations.len())),
                (Tab::Queue, format!("queue {queued}")),
                (Tab::History, format!("history {}", self.history.len())),
            ];
            let opts: Vec<(Tab, &str)> = opts.iter().map(|(t, s)| (*t, s.as_str())).collect();
            ui.allocate_ui(Vec2::new(360.0, 24.0), |ui| tabs(ui, &mut self.tab, &opts));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(Key::new("add printer").outline(VALUE)).clicked() {
                    acts.push(Act::Edit(None));
                }
                if ui.add(Key::new("queue g-code").outline(READOUT)).clicked() {
                    acts.push(Act::Pick(None));
                }
            });
        });
    }

    fn printers(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        if self.stations.is_empty() {
            section(ui, "no printers yet", "", |ui| {
                note(
                    ui,
                    "Add a printer to see its state, temperatures and camera here. A Marlin printer on a USB cable is found by its serial port; the speed can be detected.",
                    LEGEND,
                );
                ui.add_space(8.0);
                if ui.add(Key::new("add printer").solid(READOUT)).clicked() {
                    acts.push(Act::Edit(None));
                }
            });
            return;
        }
        let cols = ((ui.available_width() / CARD_W).floor() as usize).clamp(1, self.stations.len());
        let now = SystemTime::now();
        ui.columns(cols, |columns| {
            for i in 0..self.stations.len() {
                let ui = &mut columns[i % cols];
                self.printer_card(ui, i, now, acts);
                ui.add_space(10.0);
            }
        });
    }

    fn camera_texture(
        &mut self,
        ctx: &egui::Context,
        id: &str,
    ) -> (Option<(egui::TextureId, Vec2)>, Option<String>) {
        let Some(cam) = self.cameras.get(id) else { return (None, None) };
        let Ok(feed) = cam.feed.lock() else { return (None, None) };
        let error = feed.error.clone();
        if let Some(f) = &feed.frame {
            let image = || egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
            match self.frames.get_mut(id) {
                Some((seq, tex)) if *seq != f.seq => {
                    tex.set(image(), TextureOptions::LINEAR);
                    *seq = f.seq;
                }
                Some(_) => {}
                None => {
                    let tex =
                        ctx.load_texture(format!("camera {id}"), image(), TextureOptions::LINEAR);
                    self.frames.insert(id.to_string(), (f.seq, tex));
                }
            }
        }
        let tex = self.frames.get(id).map(|(_, t)| (t.id(), t.size_vec2()));
        (tex, error)
    }

    fn printer_card(&mut self, ui: &mut Ui, i: usize, now: SystemTime, acts: &mut Vec<Act>) {
        let station = &self.stations[i];
        let config = station.config.clone();
        let status = station.status();
        let samples = station.shared.lock().map(|s| heater_series(&s.samples)).unwrap_or_default();
        let refused = station.shared.lock().ok().and_then(|mut s| s.refused.take());
        if let Some(r) = refused {
            self.notice = Some((false, format!("{}: {r}", config.name)));
        }
        let state = status.state();
        let tint = state_tint(state);
        let card_data = CardData {
            i,
            config: config.clone(),
            status,
            samples,
            unclear: self.queue.unclear.contains(&config.id),
            next: self.queue.next_for(&config.id).cloned(),
            now,
        };
        let (frame, cam_error) = self.camera_texture(ui.ctx(), &config.id);
        let has_camera = config.camera.is_some();
        card(
            ui,
            Some(tint),
            |ui| {
                Line::new().legend(&config.name).show(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    chip(ui, state.label(), tint, state != State::Offline);
                });
            },
            |ui| {
                let mut opened = false;
                if has_camera && ui.available_width() >= SIDE_CAMERA_W {
                    ui.horizontal_top(|ui| {
                        let cam_w = (ui.available_width() * 0.45).min(480.0);
                        let info_w = ui.available_width() - cam_w - 12.0;
                        ui.allocate_ui_with_layout(
                            Vec2::new(info_w, 0.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| printer_info(ui, &card_data, acts),
                        );
                        ui.add_space(4.0);
                        ui.allocate_ui_with_layout(
                            Vec2::new(cam_w, 0.0),
                            egui::Layout::top_down(egui::Align::Max),
                            |ui| {
                                opened = camera_view(ui, frame, cam_error.as_deref(), cam_w * 0.75)
                            },
                        );
                    });
                } else {
                    if has_camera {
                        let h = (ui.available_width() * 9.0 / 16.0).min(240.0);
                        opened = camera_view(ui, frame, cam_error.as_deref(), h);
                    }
                    printer_info(ui, &card_data, acts);
                }
                if opened {
                    acts.push(Act::Camera(i));
                }
            },
        );
    }

    fn thumb(&mut self, ctx: &egui::Context, path: &Path) -> Option<egui::TextureId> {
        let t = self.thumbs.entry(path.to_path_buf()).or_insert_with(|| Thumb {
            texture: gcode::thumbnail(path).map(|t| {
                ctx.load_texture(
                    format!("thumb {}", path.display()),
                    egui::ColorImage::from_rgba_unmultiplied([t.width, t.height], &t.rgba),
                    TextureOptions::LINEAR,
                )
            }),
        });
        t.texture.as_ref().map(|t| t.id())
    }

    fn queue_tab(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let jobs = self.queue.jobs.clone();
        let printers: Vec<(Option<String>, String)> =
            std::iter::once((None, "any printer".to_string()))
                .chain(self.config.printers.iter().map(|p| (Some(p.id.clone()), p.name.clone())))
                .collect();
        section(ui, "queue", "a printer offers the first job here that it can take", |ui| {
            if jobs.is_empty() {
                note(
                    ui,
                    "The queue is empty. Drop G-code files on the window, or use queue g-code.",
                    LEGEND,
                );
                return;
            }
            for (n, job) in jobs.iter().enumerate() {
                if n > 0 {
                    let y = ui.cursor().top();
                    let r = ui.max_rect();
                    ui.painter().line_segment(
                        [egui::pos2(r.left(), y), egui::pos2(r.right(), y)],
                        Stroke::new(1.0, ETCH),
                    );
                    ui.add_space(4.0);
                }
                ui.horizontal(|ui| {
                    let size = Vec2::new(72.0, 54.0);
                    match self.thumb(ui.ctx(), &job.path) {
                        Some(tex) => {
                            ui.add(
                                egui::Image::new((tex, size))
                                    .maintain_aspect_ratio(true)
                                    .fit_to_exact_size(size),
                            );
                        }
                        None => {
                            let (r, _) = ui.allocate_exact_size(size, Sense::hover());
                            ui.painter().rect_filled(r, 2.0, WELL);
                            ui.painter().text(
                                r.center(),
                                egui::Align2::CENTER_CENTER,
                                "G",
                                legend_font(20.0),
                                ETCH,
                            );
                        }
                    }
                    ui.vertical(|ui| {
                        ui.set_width((ui.available_width() - 340.0).max(160.0));
                        Line::new().value(&job.name).size(14.0).elided(ui);
                        Line::new().note(job_summary(&job.meta)).size(10.5).elided(ui);
                        let missing = !job.path.exists();
                        Line::new()
                            .note(if missing {
                                format!("missing: {}", job.path.display())
                            } else {
                                job.path.display().to_string()
                            })
                            .tint(if missing { FAULT } else { LEGEND })
                            .size(10.0)
                            .elided(ui);
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(Key::new("remove").outline(FAULT)).clicked() {
                            acts.push(Act::Drop(job.id));
                        }
                        if ui
                            .add(Key::new("↓").outline(VALUE).enabled(n + 1 < jobs.len()))
                            .clicked()
                        {
                            acts.push(Act::Shift(job.id, 1));
                        }
                        if ui.add(Key::new("↑").outline(VALUE).enabled(n > 0)).clicked() {
                            acts.push(Act::Shift(job.id, -1));
                        }
                        ui.allocate_ui(Vec2::new(150.0, 24.0), |ui| {
                            let mut pick = job.printer.clone();
                            if choice(ui, ("assign", job.id), &mut pick, printers.clone()) {
                                acts.push(Act::Assign(job.id, pick));
                            }
                        });
                    });
                });
            }
        });
    }

    fn history_tab(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let done: Vec<&Record> =
            self.history.iter().filter(|r| r.outcome == Outcome::Done).collect();
        let hours = done.iter().map(|r| r.seconds()).sum::<u64>() as f64 / 3600.0;
        let grams = done.iter().filter_map(|r| r.filament_g).fold(0.0, |a, g| a + g);
        let failed = self.history.iter().filter(|r| r.outcome == Outcome::Failed).count();
        section(ui, "history", "right click a print to queue it again", |ui| {
            readouts(
                ui,
                &[
                    ("printed", done.len().to_string(), VALUE),
                    ("hours", format!("{hours:.1}"), VALUE),
                    (
                        "filament",
                        if grams < 1000.0 {
                            format!("{grams:.0} g")
                        } else {
                            format!("{:.2} kg", grams / 1000.0)
                        },
                        VALUE,
                    ),
                    ("failed", failed.to_string(), if failed > 0 { FAULT } else { VALUE }),
                ],
            );
            if self.history.is_empty() {
                note(ui, "Finished, cancelled and failed prints are listed here.", LEGEND);
                return;
            }
            let rest = (ui.available_width() - 670.0).max(160.0);
            let cols = [
                ("ended", 110.0),
                ("printer", 140.0),
                ("job", 260.0),
                ("time", 80.0),
                ("filament", 80.0),
                ("outcome", rest),
            ];
            let head =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover()).0;
            egui_bench::table::header(ui.painter(), head, &cols);
            let n = self.history.len();
            egui::ScrollArea::vertical().auto_shrink([false, true]).show_rows(
                ui,
                ROW_H,
                n,
                |ui, range| {
                    for k in range {
                        let i = n - 1 - k;
                        let r = &self.history[i];
                        let (row, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), ROW_H),
                            Sense::click(),
                        );
                        let p = ui.painter_at(row);
                        if resp.hovered() {
                            p.rect_filled(row, 0.0, BAND);
                        } else if k % 2 == 1 {
                            p.rect_filled(row, 0.0, WELL.gamma_multiply(0.6));
                        }
                        let mut x = row.left() + 6.0;
                        let tint = match r.outcome {
                            Outcome::Done => OK,
                            Outcome::Cancelled => WARN,
                            Outcome::Failed => FAULT,
                        };
                        let outcome = if r.reason.is_empty() || r.outcome == Outcome::Cancelled {
                            r.outcome.label().to_string()
                        } else {
                            format!("{}: {}", r.outcome.label(), r.reason)
                        };
                        let fields = [
                            (when(r.ended), VALUE),
                            (r.printer.clone(), VALUE),
                            (r.job.clone(), VALUE),
                            (
                                if r.seconds() == 0 {
                                    "-".into()
                                } else {
                                    hms(Duration::from_secs(r.seconds()))
                                },
                                VALUE,
                            ),
                            (r.filament_g.map(|g| format!("{g:.1} g")).unwrap_or_default(), VALUE),
                            (outcome, tint),
                        ];
                        for ((text, col), (_, w)) in fields.iter().zip(cols) {
                            cell(&p, row, x, w, text, *col);
                            x += w;
                        }
                        let path = r.path.clone();
                        resp.context_menu(|ui| {
                            if ui
                                .add_enabled(
                                    path.exists(),
                                    egui::Button::new(action("Queue again")),
                                )
                                .clicked()
                            {
                                acts.push(Act::Again(i));
                                ui.close();
                            }
                        });
                    }
                },
            );
        });
    }

    fn editor(&mut self, ctx: &egui::Context) {
        let Some(ed) = self.editor.as_mut() else { return };
        if let Some(rx) = &ed.detecting
            && let Ok(r) = rx.try_recv()
        {
            ed.detecting = None;
            ed.found = Some(match r {
                Ok(d) => {
                    ed.baud = d.baud;
                    if ed.name.trim().is_empty()
                        && let Some(m) = &d.machine
                    {
                        ed.name = m.clone();
                    }
                    let what = d.firmware.or(d.machine).unwrap_or_else(|| "a printer".into());
                    (true, format!("{what} answered at {} baud", d.baud))
                }
                Err(e) => (false, e),
            });
        }
        let mut save = false;
        let mut close = false;
        let modal = egui::Modal::new(Id::new("printer editor")).show(ctx, |ui| {
            ui.set_width(460.0);
            modal_title(ui, if ed.index.is_some() { "Edit printer" } else { "Add a printer" });
            row(ui, "name", |ui| {
                field(ui, &mut ed.name, "Ender-3 Pro");
            });
            row_help(ui, "connection", "Marlin over USB streams the print from this computer, so it stops if printfarm is closed.", |ui| {
                let mut kind = 0;
                choice(ui, "kind", &mut kind, [(0, "Marlin over USB".to_string())]);
            });
            row(ui, "port", |ui| {
                let mut options: Vec<(String, String)> = ed.ports.iter().map(|p| (p.path.clone(), p.label.clone())).collect();
                if !ed.port.is_empty() && !options.iter().any(|(p, _)| *p == ed.port) {
                    options.push((ed.port.clone(), ed.port.clone()));
                }
                if options.is_empty() {
                    Line::new().note("no serial ports found").show(ui);
                } else {
                    choice(ui, "port", &mut ed.port, options);
                }
            });
            row(ui, "", |ui| {
                field(ui, &mut ed.port, "/dev/ttyUSB0");
            });
            row(ui, "speed", |ui| {
                ui.horizontal(|ui| {
                    ui.allocate_ui(Vec2::new(140.0, 24.0), |ui| {
                        choice(ui, "baud", &mut ed.baud, BAUDS.map(|b| (b, format!("{b} baud"))));
                    });
                    let busy = ed.detecting.is_some();
                    if ui.add(Key::new(if busy { "detecting…" } else { "detect" }).outline(READOUT).enabled(!busy && !ed.port.is_empty())).clicked() {
                        let (tx, rx) = channel();
                        let port = ed.port.clone();
                        let c = ui.ctx().clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(marlin::detect(&port));
                            c.request_repaint();
                        });
                        ed.detecting = Some(rx);
                        ed.found = None;
                    }
                });
            });
            if let Some((ok, text)) = &ed.found {
                status(ui, *ok, text);
            }
            row_help(ui, "camera", "A snapshot or MJPEG stream URL, such as http://printer.local/webcam/?action=stream. Leave it empty for none.", |ui| {
                field(ui, &mut ed.camera, "http://…/snapshot.jpg");
            });
            footer(ui, |ui| {
                let ready = !ed.name.trim().is_empty() && !ed.port.trim().is_empty() && ed.detecting.is_none();
                if ui.add(Key::new("save").solid(READOUT).enabled(ready).min_width(100.0)).clicked() {
                    save = true;
                }
                if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if save {
            let ed = self.editor.take().unwrap();
            let link = Link::MarlinUsb { port: ed.port.trim().to_string(), baud: ed.baud };
            let camera = Some(ed.camera.trim().to_string()).filter(|c| !c.is_empty());
            let name = ed.name.trim().to_string();
            match ed.index {
                Some(i) => {
                    let p = &mut self.config.printers[i];
                    p.name = name;
                    p.link = link;
                    p.camera = camera;
                    let p = p.clone();
                    self.stations[i] = self.station(p);
                }
                None => {
                    let id = self.config.fresh_id(&name);
                    let p = PrinterConfig { id, name, link, camera };
                    let station = self.station(p.clone());
                    self.stations.push(station);
                    self.config.printers.push(p);
                }
            }
            self.save_config();
            self.sync_cameras();
        } else if close {
            self.editor = None;
        }
    }

    fn confirmations(&mut self, ctx: &egui::Context) {
        if let Some(i) = self.removing {
            let name = self.config.printers[i].name.clone();
            let mut done = false;
            let m = egui::Modal::new(Id::new("remove")).show(ctx, |ui| {
                ui.set_width(380.0);
                modal_title(ui, &format!("Remove {name}?"));
                note(ui, "Its queued jobs go back to any printer. History is kept.", LEGEND);
                footer(ui, |ui| {
                    if ui.add(Key::new("remove").solid(FAULT).min_width(100.0)).clicked() {
                        let id = self.config.printers.remove(i).id;
                        self.stations.remove(i);
                        for j in
                            self.queue.jobs.iter_mut().filter(|j| j.printer.as_deref() == Some(&id))
                        {
                            j.printer = None;
                        }
                        self.queue.unclear.remove(&id);
                        done = true;
                    }
                    if ui.add(Key::new("keep it").outline(VALUE).min_width(100.0)).clicked() {
                        self.removing = None;
                    }
                });
            });
            if done {
                self.removing = None;
                self.save_config();
                self.save_queue();
                self.sync_cameras();
            } else if m.should_close() {
                self.removing = None;
            }
        }
        if let Some(i) = self.cancelling {
            let name = self.config.printers[i].name.clone();
            let progress = self.stations[i].status().progress;
            let sending = progress.as_ref().is_some_and(|p| p.sending);
            let job = progress.map(|p| p.job.name).unwrap_or_default();
            let m = egui::Modal::new(Id::new("cancel")).show(ctx, |ui| {
                ui.set_width(380.0);
                modal_title(ui, &format!("Cancel {job}?"));
                let said = if sending {
                    format!(
                        "{name} stops receiving it and the job goes back to the top of the queue."
                    )
                } else {
                    format!("{name} stops, and its heaters and fan are turned off.")
                };
                note(ui, said, LEGEND);
                footer(ui, |ui| {
                    if ui.add(Key::new("cancel print").solid(FAULT).min_width(120.0)).clicked() {
                        self.stations[i].send(Control::Cancel);
                        self.cancelling = None;
                    }
                    if ui.add(Key::new("keep printing").outline(VALUE).min_width(120.0)).clicked() {
                        self.cancelling = None;
                    }
                });
            });
            if m.should_close() {
                self.cancelling = None;
            }
        }
        if let Some((i, heat)) = self.probing {
            let name = self.config.printers[i].name.clone();
            let mut pick = heat;
            let mut go = false;
            let m = egui::Modal::new(Id::new("probe")).show(ctx, |ui| {
                ui.set_width(400.0);
                modal_title(ui, &format!("Probe the bed on {name}?"));
                note(
                    ui,
                    "It homes and then touches the bed at every mesh point. Make sure the bed is empty. Some firmware heats the nozzle and bed itself before probing; the heaters go back to how they were once it is done.",
                    LEGEND,
                );
                ui.add_space(6.0);
                row_help(ui, "bed", "A bed moves as it heats, so probe it at the temperature you print at.", |ui| {
                    choice(ui, "probe heat", &mut pick, [
                        (0, "no extra heat".to_string()),
                        (60, "60 °C, PLA".to_string()),
                        (65, "65 °C".to_string()),
                        (80, "80 °C, PETG".to_string()),
                    ]);
                });
                footer(ui, |ui| {
                    if ui.add(Key::new("probe").solid(READOUT).min_width(100.0)).clicked() {
                        go = true;
                    }
                    if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                        self.probing = None;
                    }
                });
            });
            if go {
                let status = self.stations[i].status();
                let target = |name: &str| status.heater(name).map_or(0.0, |h| h.target);
                let gcode = probe_gcode(pick, target("hotend"), target("bed"));
                self.stations[i].send(Control::Gcode(gcode));
                self.probing = None;
            } else if m.should_close() {
                self.probing = None;
            } else if self.probing.is_some() {
                self.probing = Some((i, pick));
            }
        }
        if let Some(i) = self.saving_mesh {
            let name = self.config.printers[i].name.clone();
            let m = egui::Modal::new(Id::new("save mesh")).show(ctx, |ui| {
                ui.set_width(400.0);
                modal_title(ui, &format!("Save the mesh on {name}?"));
                note(
                    ui,
                    "M500 writes every setting the printer holds to its EEPROM, not only the mesh, and they load at every power on.",
                    WARN,
                );
                footer(ui, |ui| {
                    if ui.add(Key::new("save").solid(WARN).min_width(100.0)).clicked() {
                        self.stations[i].send(Control::Gcode("M500".into()));
                        self.saving_mesh = None;
                    }
                    if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                        self.saving_mesh = None;
                    }
                });
            });
            if m.should_close() {
                self.saving_mesh = None;
            }
        }
        if self.closing {
            let sending = self.sending();
            let m = egui::Modal::new(Id::new("closing")).show(ctx, |ui| {
                ui.set_width(400.0);
                modal_title(ui, "Still sending to a printer");
                note(
                    ui,
                    format!(
                        "{} is still being copied to the printer. Closing printfarm stops the copy and puts it back at the top of the queue. Prints already on a printer carry on.",
                        sending.join(", ")
                    ),
                    WARN,
                );
                footer(ui, |ui| {
                    if ui.add(Key::new("close anyway").solid(FAULT).min_width(140.0)).clicked() {
                        self.closing = false;
                        self.stations.clear();
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.add(Key::new("keep sending").outline(VALUE).min_width(120.0)).clicked() {
                        self.closing = false;
                    }
                });
            });
            if m.should_close() {
                self.closing = false;
            }
        }
    }

    fn console_window(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let Some(id) = self.console.clone() else { return };
        let Some(i) = self.config.printers.iter().position(|p| p.id == id) else {
            self.console = None;
            return;
        };
        let mut open = true;
        let shared = self.stations[i].shared.clone();
        egui::Window::new(format!("{} console", self.config.printers[i].name))
            .id(Id::new(("console", &id)))
            .open(&mut open)
            .default_size([560.0, 380.0])
            .show(ctx, |ui| {
                let mut send = false;
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 80.0;
                    let r = ui.add_sized(
                        [w, 24.0],
                        egui::TextEdit::singleline(&mut self.console_line)
                            .hint_text("G-code, such as M503")
                            .font(FontId::monospace(13.0)),
                    );
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        send = true;
                        r.request_focus();
                    }
                    if ui.add(Key::new("send").outline(READOUT)).clicked() {
                        send = true;
                    }
                });
                if send && !self.console_line.trim().is_empty() {
                    acts.push(Act::Gcode(i, std::mem::take(&mut self.console_line)));
                }
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        let Ok(s) = shared.lock() else { return };
                        for (dir, text) in &s.console {
                            let (prefix, col) = match dir {
                                Dir::Sent => ("> ", READOUT),
                                Dir::Received => ("  ", VALUE),
                                Dir::Note => ("# ", LEGEND),
                            };
                            ui.label(
                                egui::RichText::new(format!("{prefix}{text}"))
                                    .font(mono(12.0))
                                    .color(col),
                            );
                        }
                    });
            });
        if !open {
            self.console = None;
        }
    }

    fn mesh_window(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let Some(id) = self.mesh.clone() else { return };
        let Some(i) = self.config.printers.iter().position(|p| p.id == id) else {
            self.mesh = None;
            return;
        };
        let status = self.stations[i].status();
        let unclear = self.queue.unclear.contains(&id);
        let mut open = true;
        egui::Window::new(format!("{} bed", self.config.printers[i].name))
            .id(Id::new(("mesh", &id)))
            .open(&mut open)
            .default_width(460.0)
            .resizable(false)
            .show(ctx, |ui| {
                let l = &status.leveling;
                let span = l.mesh.as_ref().and_then(|m| m.span());
                let enabled = match l.enabled {
                    Some(true) => ("on".to_string(), OK),
                    Some(false) => ("off".to_string(), WARN),
                    None => ("-".to_string(), LEGEND),
                };
                readouts(
                    ui,
                    &[
                        ("leveling", enabled.0, enabled.1),
                        (
                            "range",
                            span.map(|(lo, hi)| format!("{:.3} mm", hi - lo)).unwrap_or("-".into()),
                            TRACE,
                        ),
                        (
                            "lowest",
                            span.map(|(lo, _)| format!("{lo:+.3}")).unwrap_or("-".into()),
                            TRACE,
                        ),
                        (
                            "highest",
                            span.map(|(_, hi)| format!("{hi:+.3}")).unwrap_or("-".into()),
                            TRACE,
                        ),
                        ("fade", l.fade.map(|f| format!("{f:.0} mm")).unwrap_or("-".into()), VALUE),
                    ],
                );
                match &l.mesh {
                    Some(m) => {
                        mesh_map(ui, m);
                        let when = when(farm::unix(m.read));
                        let what = if m.probed { "probed" } else { "read from the printer" };
                        Line::new()
                            .note(format!("{what} {when}. Positive is where the bed is high."))
                            .size(10.5)
                            .wrapped(ui);
                    }
                    None => note(
                        ui,
                        "The printer has not reported a mesh. Probe the bed to make one.",
                        LEGEND,
                    ),
                }
                if let Some(a) = status.activity {
                    Line::new().legend("now").value(a).show(ui);
                }
                ui.add_space(8.0);
                let idle = status.state() == State::Idle && status.activity.is_none();
                ui.horizontal(|ui| {
                    if ui.add(Key::new("probe").solid(READOUT).enabled(idle && !unclear)).clicked()
                    {
                        acts.push(Act::Probe(i));
                    }
                    let (label, cmd) = if l.enabled == Some(true) {
                        ("leveling off", "M420 S0")
                    } else {
                        ("leveling on", "M420 S1")
                    };
                    if ui
                        .add(Key::new(label).outline(VALUE).enabled(l.mesh.is_some() && idle))
                        .clicked()
                    {
                        acts.push(Act::Gcode(i, cmd.into()));
                    }
                    if ui.add(Key::new("read again").outline(VALUE).enabled(idle)).clicked() {
                        acts.push(Act::Gcode(i, "M420 V".into()));
                    }
                    if ui
                        .add(Key::new("save").outline(WARN).enabled(l.mesh.is_some() && idle))
                        .clicked()
                    {
                        acts.push(Act::SaveMesh(i));
                    }
                });
                if unclear {
                    note(ui, "Clear the bed and mark it clear on the card before probing.", WARN);
                }
            });
        if !open {
            self.mesh = None;
        }
    }

    fn big_camera_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.big_camera.clone() else { return };
        let (frame, _) = self.camera_texture(ctx, &id);
        let Some((tex, size)) = frame else {
            self.big_camera = None;
            return;
        };
        let mut open = true;
        egui::Window::new(format!("{} camera", self.printer_name(&id)))
            .id(Id::new(("camera", &id)))
            .open(&mut open)
            .default_size([size.x.min(960.0), size.y.min(720.0)])
            .show(ctx, |ui| {
                let avail = ui.available_size();
                let scale = (avail.x / size.x).min(avail.y / size.y).max(0.1);
                ui.add(egui::Image::new((tex, size * scale)));
            });
        if !open {
            self.big_camera = None;
        }
    }

    fn files(&mut self, ctx: &egui::Context) {
        if let Some((b, printer)) = &mut self.browser {
            match b.show(ctx) {
                Picked::Browsing => {}
                Picked::Cancelled | Picked::Folder(_) => self.browser = None,
                Picked::File(p) => {
                    let printer = printer.clone();
                    self.browser = None;
                    self.enqueue(&p, printer);
                }
            }
        }
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for p in dropped {
            self.enqueue(&p, None);
        }
        if let Some(hovered) = egui_bench::drop::hovering(ctx) {
            let paths: Vec<PathBuf> = hovered.into_iter().flatten().collect();
            let good = paths.iter().filter(|p| gcode::is_gcode(p)).count();
            let (head, tint) = match (paths.len(), good) {
                (0, _) => ("Drop to queue".to_string(), READOUT),
                (1, 1) => (
                    format!("Queue {}", paths[0].file_name().unwrap_or_default().to_string_lossy()),
                    READOUT,
                ),
                (_, 0) => ("Only G-code files can be queued".to_string(), FAULT),
                (_, n) => (format!("Queue {n} files"), READOUT),
            };
            egui_bench::drop::overlay(
                ctx,
                None,
                &head,
                "Each one goes to the end of the queue, for any printer",
                tint,
            );
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.events();
        if ctx.input(|i| i.viewport().close_requested())
            && !self.sending().is_empty()
            && !self.closing
        {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        let mut acts = Vec::new();
        egui::Panel::top("header")
            .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 8,
                bottom: 6,
            }))
            .show(ui, |ui| self.header(ui, &mut acts));
        if let Some((ok, text)) = self.notice.clone() {
            egui::Panel::bottom("notice")
                .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(egui::Margin::symmetric(12, 4)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        status(ui, ok, &text);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("x").clicked() {
                                self.notice = None;
                            }
                        });
                    });
                });
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let r = ui.max_rect();
            ui.painter().rect_filled(r, 0.0, CHASSIS);
            ui.painter().line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, ETCH));
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::NONE.inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                    match self.tab {
                        Tab::Printers => self.printers(ui, &mut acts),
                        Tab::Queue => self.queue_tab(ui, &mut acts),
                        Tab::History => self.history_tab(ui, &mut acts),
                    }
                });
            });
        });
        self.console_window(&ctx, &mut acts);
        self.big_camera_window(&ctx);
        self.mesh_window(&ctx, &mut acts);
        self.apply(acts);
        self.editor(&ctx);
        self.confirmations(&ctx);
        self.files(&ctx);
        if self.busy() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}

const SIDE_CAMERA_W: f32 = 560.0;

struct CardData {
    i: usize,
    config: PrinterConfig,
    status: crate::printer::Status,
    samples: HashMap<String, Vec<f32>>,
    unclear: bool,
    next: Option<crate::printer::Job>,
    now: SystemTime,
}

fn camera_view(
    ui: &mut Ui,
    frame: Option<(egui::TextureId, Vec2)>,
    error: Option<&str>,
    max_h: f32,
) -> bool {
    let Some((tex, size)) = frame else {
        note(ui, error.unwrap_or("waiting for the camera"), LEGEND);
        return false;
    };
    let scale = (ui.available_width() / size.x).min(max_h / size.y);
    ui.add(egui::Image::new((tex, size * scale)).sense(Sense::click()))
        .on_hover_text("open the camera")
        .clicked()
}

fn heater_line(ui: &mut Ui, status: &crate::printer::Status, samples: &HashMap<String, Vec<f32>>) {
    if status.heaters.is_empty() {
        return;
    }
    ui.add_space(6.0);
    let n = status.heaters.len() as f32;
    let gap = 8.0;
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::hover());
    let w = (row.width() - gap * (n - 1.0)) / n;
    for (k, h) in status.heaters.iter().enumerate() {
        let cell = Rect::from_min_size(
            egui::pos2(row.left() + k as f32 * (w + gap), row.top()),
            Vec2::new(w, row.height()),
        );
        let points = samples.get(&h.name).map(Vec::as_slice).unwrap_or_default();
        let top = points.iter().copied().fold(h.target.max(50.0), f32::max) * 1.05;
        let band = 18.0;
        ui.painter().rect_filled(cell, 1.0, WELL);
        ui.painter().rect_stroke(cell, 1.0, Stroke::new(1.0, ETCH), egui::StrokeKind::Inside);
        let plot = Rect::from_min_max(egui::pos2(cell.left(), cell.top() + band), cell.max);
        let mut t = Trace::new(points).size(plot.size()).range(0.0, top);
        if h.target > 0.0 {
            t = t.rule(h.target);
        }
        ui.put(plot, t);
        let mut line = Line::new().legend(&h.name).measured(format!("{:.1} °C", h.actual));
        if h.target > 0.0 {
            line = line.note(" / ").set(format!("{:.0}", h.target));
        }
        let galley = ui.painter().layout_job(line.job());
        ui.painter_at(cell).galley(cell.left_top() + Vec2::new(6.0, 3.0), galley, VALUE);
    }
}

fn probe_gcode(bed: u32, hotend_was: f32, bed_was: f32) -> String {
    let mut g = Vec::new();
    if bed > 0 {
        g.push(format!("M190 S{bed}"));
    }
    g.extend(["G28".to_string(), "G29".to_string(), "M420 V".to_string()]);
    g.push(format!("M104 S{hotend_was:.0}"));
    g.push(format!("M140 S{bed_was:.0}"));
    g.join("\n")
}

fn leveling_summary(l: &crate::printer::mesh::Leveling) -> Option<String> {
    let on = match l.enabled? {
        true => "leveling on",
        false => "leveling off",
    };
    Some(match l.mesh.as_ref().and_then(|m| m.span().map(|s| (m, s))) {
        Some((m, (lo, hi))) => {
            format!("{on}, {} x {}, {:.2} mm range", m.columns(), m.rows.len(), hi - lo)
        }
        None => format!("{on}, no mesh"),
    })
}

fn mesh_colour(v: f32, reach: f32) -> Color32 {
    let t = (v.abs() / reach.max(0.01)).clamp(0.0, 1.0);
    let far = if v < 0.0 { TRACE } else { READOUT };
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgb(lerp(WELL.r(), far.r()), lerp(WELL.g(), far.g()), lerp(WELL.b(), far.b()))
}

fn mesh_map(ui: &mut Ui, m: &crate::printer::mesh::Mesh) {
    let (cols, rows) = (m.columns().max(1), m.rows.len().max(1));
    let reach = m.values().fold(0.0f32, |a, v| a.max(v.abs()));
    let w = ui.available_width();
    let cell = Vec2::new(w / cols as f32, (w / cols as f32 * 0.5).clamp(22.0, 40.0));
    let label = 14.0;
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(w, cell.y * rows as f32 + label * 2.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.text(
        egui::pos2(rect.center().x, rect.top() + label / 2.0),
        egui::Align2::CENTER_CENTER,
        "BACK",
        legend_font(10.0),
        LEGEND,
    );
    p.text(
        egui::pos2(rect.center().x, rect.bottom() - label / 2.0),
        egui::Align2::CENTER_CENTER,
        "FRONT",
        legend_font(10.0),
        LEGEND,
    );
    for (y, row) in m.rows.iter().enumerate() {
        for (x, v) in row.iter().enumerate() {
            let top = rect.top() + label + (rows - 1 - y) as f32 * cell.y;
            let r = Rect::from_min_size(egui::pos2(rect.left() + x as f32 * cell.x, top), cell)
                .shrink(1.0);
            let (fill, text) = match v {
                Some(v) => (mesh_colour(*v, reach), format!("{v:+.3}")),
                None => (WELL, "-".into()),
            };
            p.rect_filled(r, 2.0, fill);
            let ink =
                if v.is_some_and(|v| v.abs() / reach.max(0.01) > 0.55) { CHASSIS } else { VALUE };
            p.text(r.center(), egui::Align2::CENTER_CENTER, text, mono(11.0), ink);
        }
    }
}

fn progress_hero(ui: &mut Ui, label: &str, fraction: f32, tint: Color32) {
    hero(ui, label, &format!("{:.1}", fraction * 100.0), "%", tint);
    let r = ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::hover()).0;
    bar(ui.painter(), r, fraction, tint);
    ui.add_space(4.0);
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn printer_info(ui: &mut Ui, d: &CardData, acts: &mut Vec<Act>) {
    let (i, config, status, now) = (d.i, &d.config, &d.status, d.now);
    let (unclear, next) = (d.unclear, &d.next);
    let state = status.state();
    let what = [status.machine.clone(), status.firmware.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    Line::new()
        .note(if what.is_empty() { config.link.summary() } else { what })
        .size(10.5)
        .elided(ui);
    ui.add_space(2.0);
    match state {
        State::Offline | State::Connecting => {
            note(ui, &status.detail, if state == State::Offline { WARN } else { LEGEND });
            if let Some(p) = &status.progress {
                Line::new().legend("on the printer").value(&p.job.name).elided(ui);
                Line::new()
                    .note("printfarm checks on it when the printer answers")
                    .size(10.5)
                    .elided(ui);
            }
        }
        State::Fault => {
            note(ui, &status.detail, FAULT);
            ui.add_space(4.0);
            if ui.add(Key::new("reconnect").outline(VALUE)).clicked() {
                acts.push(Act::Reconnect(i));
            }
        }
        State::Printing | State::Paused => {
            let tint = if state == State::Paused { WARN } else { TRACE };
            if let Some(p) = status.progress.as_ref().filter(|p| p.sending) {
                progress_hero(ui, &format!("sending {}", p.job.name), p.fraction(), TRACE);
                readouts(
                    ui,
                    &[
                        ("sent", format!("{} / {}", megabytes(p.done), megabytes(p.total)), VALUE),
                        ("left", p.remaining(now).map(hms).unwrap_or("-".into()), VALUE),
                    ],
                );
                note(
                    ui,
                    "Copying it to the printer's SD card. The printer starts it once the copy is done.",
                    LEGEND,
                );
                ui.add_space(4.0);
                if ui.add(Key::new("cancel").outline(FAULT).min_width(100.0)).clicked() {
                    acts.push(Act::Cancel(i));
                }
            } else if let Some(p) = &status.progress {
                progress_hero(ui, &p.job.name, p.fraction(), tint);
                let layers = if p.job.meta.layers > 0 {
                    format!("{} / {}", p.layer, p.job.meta.layers)
                } else {
                    p.layer.to_string()
                };
                readouts(
                    ui,
                    &[
                        ("elapsed", hms(p.elapsed(now)), VALUE),
                        ("left", p.remaining(now).map(hms).unwrap_or("-".into()), VALUE),
                        ("layer", layers, VALUE),
                    ],
                );
                if let Some(a) = status.activity {
                    Line::new().legend("now").value(a).elided(ui);
                }
                if let Some(m) = &status.message {
                    Line::new().legend("says").value(m).elided(ui);
                }
                Line::new()
                    .note("printing from its SD card, it carries on if printfarm closes")
                    .size(10.5)
                    .elided(ui);
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let (label, act) = if state == State::Paused {
                        ("resume", Act::Resume(i))
                    } else {
                        ("pause", Act::Pause(i))
                    };
                    if ui.add(Key::new(label).solid(READOUT).min_width(100.0)).clicked() {
                        acts.push(act);
                    }
                    if ui.add(Key::new("cancel").outline(FAULT).min_width(100.0)).clicked() {
                        acts.push(Act::Cancel(i));
                    }
                });
            } else if let Some(f) = &status.foreign {
                progress_hero(ui, &f.file, f.fraction(), tint);
                note(
                    ui,
                    if f.printing {
                        "Printing a file printfarm did not send. Use the printer's own screen to control it."
                    } else {
                        "Paused on a file printfarm did not send. Use the printer's own screen to control it."
                    },
                    LEGEND,
                );
            }
        }
        State::Idle => {
            if unclear {
                note(
                    ui,
                    "The last print may still be on the bed. Clear it before the next one starts.",
                    WARN,
                );
                ui.add_space(4.0);
                if ui.add(Key::new("bed is clear").solid(OK).min_width(140.0)).clicked() {
                    acts.push(Act::BedClear(i));
                }
            } else if let Some(a) = status.activity {
                Line::new().legend("now").value(a).elided(ui);
            } else if let Some(job) = &next {
                Line::new().legend("next").value(&job.name).elided(ui);
                Line::new().note(job_summary(&job.meta)).size(10.5).elided(ui);
                ui.add_space(4.0);
                if ui.add(Key::new("start").solid(READOUT).min_width(140.0)).clicked() {
                    acts.push(Act::Start(i, job.id));
                }
            } else {
                note(
                    ui,
                    "Nothing queued for this printer. Drop a G-code file on the window to queue it.",
                    LEGEND,
                );
            }
        }
    }
    heater_line(ui, status, &d.samples);
    if let Some(said) = leveling_summary(&status.leveling) {
        Line::new().legend("bed mesh").value(said).elided(ui);
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let online = !matches!(state, State::Offline | State::Connecting);
        let heat = ui.add(Key::new("heat").outline(VALUE).enabled(online));
        egui::Popup::menu(&heat).width(200.0).show(|ui| {
            for (name, hotend, bed) in PRESETS {
                if ui.button(action(format!("{name}  {hotend} / {bed} °C"))).clicked() {
                    acts.push(Act::Gcode(i, format!("M104 S{hotend}\nM140 S{bed}")));
                }
            }
            if ui.button(action("Cool down")).clicked() {
                acts.push(Act::Gcode(i, "M104 S0\nM140 S0".into()));
            }
        });
        if ui.add(Key::new("bed").outline(VALUE).enabled(online)).clicked() {
            acts.push(Act::Mesh(i));
        }
        if ui.add(Key::new("console").outline(VALUE)).clicked() {
            acts.push(Act::Console(i));
        }
        let more = ui.add(Key::more().id_salt(("more", i)));
        egui::Popup::menu(&more).width(200.0).show(|ui| {
            if ui.button(action("Queue a file for this printer…")).clicked() {
                acts.push(Act::Pick(Some(config.id.clone())));
            }
            if ui.add_enabled(!state.busy(), egui::Button::new(action("Edit"))).clicked() {
                acts.push(Act::Edit(Some(i)));
            }
            if ui.button(action("Reconnect")).clicked() {
                acts.push(Act::Reconnect(i));
            }
            if ui.add_enabled(!state.busy(), egui::Button::new(action("Remove"))).clicked() {
                acts.push(Act::Remove(i));
            }
        });
    });
}

fn state_tint(state: State) -> Color32 {
    match state {
        State::Offline => LEGEND,
        State::Connecting => WARN,
        State::Idle => OK,
        State::Printing => TRACE,
        State::Paused => WARN,
        State::Fault => FAULT,
    }
}

fn heater_series(
    samples: &std::collections::VecDeque<crate::station::Sample>,
) -> HashMap<String, Vec<f32>> {
    let mut out: HashMap<String, Vec<f32>> = HashMap::new();
    for s in samples {
        for (name, actual, _) in &s.heaters {
            out.entry(name.clone()).or_default().push(*actual);
        }
    }
    out
}

pub fn hms(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

fn when(unix: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(unix as i64, 0)
        .single()
        .map(|t| t.format("%d %b %H:%M").to_string())
        .unwrap_or_default()
}

fn job_summary(meta: &gcode::Meta) -> String {
    let mut parts = Vec::new();
    if let Some(e) = meta.estimate() {
        parts.push(hms(e));
    }
    if let Some(g) = meta.filament_g {
        parts.push(format!("{g:.1} g"));
    }
    if meta.layers > 0 {
        parts.push(format!("{} layers", meta.layers));
    }
    if let Some(s) = &meta.slicer {
        parts.push(s.clone());
    }
    if parts.is_empty() {
        parts.push(format!("{} commands", meta.commands));
    }
    parts.join(", ")
}

pub fn run(open: Vec<PathBuf>) -> eframe::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 860.0])
        .with_title("printfarm")
        .with_app_id("printfarm");
    if let Some(icon) = icon {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let opts = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "printfarm",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, open)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probing_puts_the_heaters_back() {
        assert_eq!(probe_gcode(0, 0.0, 0.0), "G28\nG29\nM420 V\nM104 S0\nM140 S0");
        assert_eq!(probe_gcode(60, 0.0, 0.0), "M190 S60\nG28\nG29\nM420 V\nM104 S0\nM140 S0");
        assert_eq!(probe_gcode(0, 150.0, 60.0), "G28\nG29\nM420 V\nM104 S150\nM140 S60");
    }
}
