#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::station::Wake;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    pub device: String,
    pub disk: String,
    pub mount: PathBuf,
    pub label: String,
    pub fs: String,
    pub size: u64,
    pub free: Option<u64>,
}

#[cfg(target_os = "linux")]
pub fn list() -> Vec<Volume> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut found = volumes(&text, Path::new("/sys"));
    for v in &mut found {
        v.free = free_space(&v.mount);
    }
    found
}

#[cfg(not(target_os = "linux"))]
pub fn list() -> Vec<Volume> {
    Vec::new()
}

struct Mount {
    source: String,
    point: PathBuf,
    fs: String,
    writable: bool,
}

fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (bytes[i], octal) {
            (b'\\', Some(d)) => {
                out.push(d.iter().fold(0u8, |a, c| a.wrapping_mul(8).wrapping_add(c - b'0')));
                i += 4;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let fields: Vec<&str> = left.split(' ').collect();
            let mut tail = right.split(' ');
            Some(Mount {
                fs: tail.next()?.to_string(),
                source: unescape(tail.next()?),
                point: PathBuf::from(unescape(fields.get(4)?)),
                writable: fields.get(5)?.split(',').any(|o| o == "rw"),
            })
        })
        .collect()
}

fn is_usb_bus(part: &str) -> bool {
    part.strip_prefix("usb").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn describe(sys: &Path, name: &str) -> Option<(String, u64)> {
    let root = sys.canonicalize().ok()?;
    let real = sys.join("class/block").join(name).canonicalize().ok()?;
    let disk_dir =
        if real.join("partition").exists() { real.parent()?.to_path_buf() } else { real.clone() };
    let read =
        |p: PathBuf| std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default();
    let removable = read(disk_dir.join("removable")) == "1";
    let sd = read(disk_dir.join("device/type")) == "SD";
    let usb = real
        .strip_prefix(&root)
        .ok()?
        .components()
        .any(|c| c.as_os_str().to_str().is_some_and(is_usb_bus));
    if !(removable || sd || usb) {
        return None;
    }
    let sectors: u64 = read(real.join("size")).parse().unwrap_or(0);
    Some((disk_dir.file_name()?.to_string_lossy().into_owned(), sectors * 512))
}

fn volumes(mountinfo: &str, sys: &Path) -> Vec<Volume> {
    let mut out: Vec<Volume> = Vec::new();
    for m in parse_mountinfo(mountinfo) {
        let skip = !m.source.starts_with("/dev/")
            || !m.writable
            || m.point == Path::new("/")
            || m.point.starts_with("/boot")
            || out.iter().any(|v| v.device == m.source);
        if skip {
            continue;
        }
        let real = std::fs::canonicalize(&m.source).unwrap_or_else(|_| PathBuf::from(&m.source));
        let Some(name) = real.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let Some((disk, size)) = describe(sys, &name) else { continue };
        let label = m
            .point
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.clone());
        out.push(Volume {
            device: m.source,
            disk,
            mount: m.point,
            label,
            fs: m.fs,
            size,
            free: None,
        });
    }
    out
}

#[cfg(unix)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut stat) } == 0)
        .then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

fn failure(output: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    text.lines().next().unwrap_or("failed").trim().to_string()
}

#[cfg(target_os = "linux")]
pub fn eject(v: &Volume) -> Result<String, String> {
    use std::process::Command;
    let _ = Command::new("sync").status();
    let udisks = Command::new("udisksctl")
        .args(["unmount", "--no-user-interaction", "-b", &v.device])
        .output();
    match udisks {
        Ok(o) if o.status.success() => {}
        other => {
            let said = match &other {
                Ok(o) => failure(o),
                Err(e) => format!("udisksctl: {e}"),
            };
            match Command::new("umount").arg(&v.mount).output() {
                Ok(o) if o.status.success() => {}
                _ => return Err(format!("{} was not ejected: {said}", v.label)),
            }
        }
    }
    Ok(format!("{} is unmounted and can be removed", v.label))
}

#[cfg(not(target_os = "linux"))]
pub fn eject(v: &Volume) -> Result<String, String> {
    Err(format!("{} cannot be ejected from printfarm on this system", v.label))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Copied {
    pub id: u64,
    pub file: String,
    pub fresh: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub copied: Vec<Copied>,
    pub failed: Option<String>,
    pub stopped: bool,
}

#[derive(Default)]
pub struct Transfer {
    total: AtomicU64,
    done: AtomicU64,
    stop: AtomicBool,
    current: Mutex<String>,
    report: Mutex<Option<Report>>,
}

impl Transfer {
    pub fn spawn(jobs: Vec<(u64, PathBuf)>, dest: PathBuf, wake: Wake) -> Arc<Transfer> {
        let transfer = Arc::new(Transfer::default());
        let t = transfer.clone();
        std::thread::spawn(move || {
            let report = run(&jobs, &dest, &t, &*wake);
            *t.report.lock().unwrap() = Some(report);
            wake();
        });
        transfer
    }

    pub fn progress(&self) -> (u64, u64, String) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
            self.current.lock().unwrap().clone(),
        )
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn report(&self) -> Option<Report> {
        self.report.lock().unwrap().take()
    }
}

enum Slot {
    Fresh(PathBuf),
    Present(PathBuf),
}

fn same_contents(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let (mut x, mut y) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

pub fn short_name(src: &Path, n: u32) -> String {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let clean: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let clean = if clean.is_empty() { "JOB".to_string() } else { clean };
    if n == 1 {
        let head: String = clean.chars().take(8).collect();
        return format!("{head}.GCO");
    }
    let tag = format!("~{n}");
    let head: String = clean.chars().take(8 - tag.len()).collect();
    format!("{head}{tag}.GCO")
}

fn existing(dest: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dest)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| file_name(p).eq_ignore_ascii_case(name))
}

fn slot(dest: &Path, src: &Path, taken: &[String]) -> std::io::Result<(String, Slot)> {
    for n in 1..=9 {
        let name = short_name(src, n);
        if taken.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
            continue;
        }
        match existing(dest, &name) {
            None => return Ok((name.clone(), Slot::Fresh(dest.join(&name)))),
            Some(found) if same_contents(src, &found)? => {
                return Ok((file_name(&found), Slot::Present(found)));
            }
            Some(_) => {}
        }
    }
    Err(std::io::Error::other("no free 8.3 name on the card"))
}

fn stem_key(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub dir: bool,
    pub size: u64,
}

pub fn read_dir(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out: Vec<Entry> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok()?;
            (!name.starts_with('.')).then(|| Entry {
                name,
                path: e.path(),
                dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

pub fn on_card(src: &Path, files: &[crate::printer::CardFile]) -> Option<String> {
    let size = std::fs::metadata(src).ok()?.len();
    let same: Vec<&crate::printer::CardFile> = files.iter().filter(|f| f.size == size).collect();
    let stem = stem_key(&src.file_stem()?.to_string_lossy());
    let alike = |f: &&crate::printer::CardFile| {
        let head = f.name.split(['~', '.']).next().map(stem_key).unwrap_or_default();
        !head.is_empty() && stem.starts_with(&head)
    };
    match same.as_slice() {
        [] => None,
        [only] => Some(only.name.clone()),
        many => {
            let named: Vec<_> = many.iter().filter(|f| alike(f)).collect();
            match named.as_slice() {
                [one] => Some(one.name.clone()),
                _ => None,
            }
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn write(src: &Path, path: &Path, t: &Transfer, wake: &dyn Fn()) -> std::io::Result<bool> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let result = (|| {
        let mut input = File::open(src)?;
        let mut out = File::create(&part)?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if t.stop.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            t.done.fetch_add(n as u64, Ordering::Relaxed);
            wake();
        }
        out.sync_all()?;
        drop(out);
        std::fs::rename(&part, path)?;
        Ok(true)
    })();
    if !matches!(result, Ok(true)) {
        let _ = std::fs::remove_file(&part);
    }
    result
}

pub fn run(jobs: &[(u64, PathBuf)], dest: &Path, t: &Transfer, wake: &dyn Fn()) -> Report {
    let mut report = Report::default();
    let mut plan = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    let mut needed = 0u64;
    for (id, src) in jobs {
        let planned = std::fs::metadata(src).and_then(|m| Ok((m.len(), slot(dest, src, &taken)?)));
        match planned {
            Ok((len, (name, slot))) => {
                if matches!(slot, Slot::Fresh(_)) {
                    needed += len;
                }
                taken.push(name);
                plan.push((*id, src, slot));
            }
            Err(e) => {
                report.failed = Some(format!("{}: {e}", src.display()));
                return report;
            }
        }
    }
    if let Some(free) = free_space(dest).filter(|free| *free < needed) {
        report.failed = Some(format!("needs {needed} bytes and the card has {free} free"));
        return report;
    }
    t.total.store(needed, Ordering::Relaxed);
    wake();
    for (id, src, slot) in plan {
        if t.stop.load(Ordering::Relaxed) {
            report.stopped = true;
            break;
        }
        match slot {
            Slot::Present(path) => {
                report.copied.push(Copied { id, file: file_name(&path), fresh: false })
            }
            Slot::Fresh(path) => {
                *t.current.lock().unwrap() = file_name(&path);
                wake();
                match write(src, &path, t, wake) {
                    Ok(true) => {
                        report.copied.push(Copied { id, file: file_name(&path), fresh: true })
                    }
                    Ok(false) => {
                        report.stopped = true;
                        break;
                    }
                    Err(e) => {
                        report.failed = Some(format!("{}: {e}", src.display()));
                        break;
                    }
                }
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("printfarm-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn idle() -> Transfer {
        Transfer::default()
    }

    #[test]
    fn mountinfo_is_read_with_escapes() {
        let text = "\
36 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
41 36 8:49 / /run/media/me/MY\\040STICK rw,nosuid shared:2 - vfat /dev/sdd1 rw,fmask=0022
42 36 0:30 / /proc rw shared:3 - proc proc rw
43 36 8:50 / /run/media/me/LOCKED ro shared:4 - vfat /dev/sde1 ro
";
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[1].point, Path::new("/run/media/me/MY STICK"));
        assert_eq!(mounts[1].source, "/dev/sdd1");
        assert_eq!(mounts[1].fs, "vfat");
        assert!(mounts[1].writable);
        assert!(!mounts[3].writable);
    }

    fn fake_sys(dir: &Path, bus: &str, disk: &str, removable: &str, kind: &str) {
        let part = dir.join(format!("devices/pci0/{bus}/block/{disk}/{disk}1"));
        std::fs::create_dir_all(&part).unwrap();
        std::fs::write(part.join("partition"), "1\n").unwrap();
        std::fs::write(part.join("size"), "30777344\n").unwrap();
        let disk_dir = part.parent().unwrap();
        std::fs::write(disk_dir.join("removable"), format!("{removable}\n")).unwrap();
        if !kind.is_empty() {
            std::fs::create_dir_all(disk_dir.join("device")).unwrap();
            std::fs::write(disk_dir.join("device/type"), format!("{kind}\n")).unwrap();
        }
        std::fs::create_dir_all(dir.join("class/block")).unwrap();
        std::os::unix::fs::symlink(&part, dir.join(format!("class/block/{disk}1"))).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn only_removable_volumes_are_found() {
        let dir = scratch("sys");
        fake_sys(&dir, "usb1/1-1", "pfa", "1", "");
        fake_sys(&dir, "usb2/2-3", "pfb", "0", "");
        fake_sys(&dir, "mmc_host/mmc0", "pfc", "0", "SD");
        fake_sys(&dir, "ata1/host0", "pfd", "0", "");
        let text = "\
1 0 8:1 / /run/media/me/STICK rw - vfat /dev/pfa1 rw
2 0 8:2 / /run/media/me/BIGUSB rw - exfat /dev/pfb1 rw
3 0 8:3 / /run/media/me/CARD rw - vfat /dev/pfc1 rw
4 0 8:4 / /data rw - ext4 /dev/pfd1 rw
5 0 8:5 / /boot rw - ext4 /dev/pfa1 rw
";
        let found = volumes(text, &dir);
        let labels: Vec<&str> = found.iter().map(|v| v.label.as_str()).collect();
        assert_eq!(labels, ["STICK", "BIGUSB", "CARD"]);
        assert_eq!(found[0].disk, "pfa");
        assert_eq!(found[0].device, "/dev/pfa1");
        assert_eq!(found[0].size, 30777344 * 512);
        assert_eq!(found[0].fs, "vfat");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn names_are_short_enough_for_the_printer() {
        assert_eq!(short_name(Path::new("/x/Benchy 3D boat.gcode"), 1), "BENCHY3D.GCO");
        assert_eq!(short_name(Path::new("/x/Benchy 3D boat.gcode"), 2), "BENCHY~2.GCO");
        assert_eq!(short_name(Path::new("/x/ab.g"), 1), "AB.GCO");
        assert_eq!(short_name(Path::new("/x/!!.gcode"), 1), "JOB.GCO");
    }

    #[test]
    fn folders_come_first_and_hidden_files_are_left_out() {
        let dir = scratch("list");
        std::fs::create_dir_all(dir.join("Zeta")).unwrap();
        std::fs::write(dir.join("b.gcode"), "G28\n").unwrap();
        std::fs::write(dir.join("A.gcode"), "G28\nG1\n").unwrap();
        std::fs::write(dir.join(".trash"), "x").unwrap();
        let list = read_dir(&dir).unwrap();
        let names: Vec<(&str, bool, u64)> =
            list.iter().map(|e| (e.name.as_str(), e.dir, e.size)).collect();
        assert_eq!(names, [("Zeta", true, 0), ("A.gcode", false, 7), ("b.gcode", false, 4)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_is_found_on_the_printers_card_by_size() {
        use crate::printer::CardFile;
        let dir = scratch("find");
        let src = dir.join("column-lower.gcode");
        std::fs::write(&src, "G28\nG1 X1\n").unwrap();
        let card = |name: &str, size: u64| CardFile { name: name.into(), size };
        let other = card("BASE~1.GCO", 99);
        assert_eq!(
            on_card(&src, &[other.clone(), card("COLUMN~1.GCO", 10)]),
            Some("COLUMN~1.GCO".into())
        );
        assert_eq!(on_card(&src, &[card("RENAMED.GCO", 10)]), Some("RENAMED.GCO".into()));
        assert_eq!(on_card(&src, &[card("COLUMN~1.GCO", 11), other.clone()]), None);
        let twins = [card("OTHER~1.GCO", 10), card("COLUMN~2.GCO", 10)];
        assert_eq!(on_card(&src, &twins), Some("COLUMN~2.GCO".into()));
        let unrelated = [card("ONE.GCO", 10), card("TWO.GCO", 10)];
        assert_eq!(on_card(&src, &unrelated), None);
        assert_eq!(on_card(&src, &[]), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copies_skip_what_is_already_on_the_card() {
        let dir = scratch("copy");
        let card = dir.join("card");
        let src = dir.join("src");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.gcode"), "G28\nG1 X1\n").unwrap();
        std::fs::write(src.join("b.gcode"), "G28\nG1 X2\n").unwrap();
        std::fs::write(card.join("b.gco"), "something else\n").unwrap();
        let jobs = vec![(1, src.join("a.gcode")), (2, src.join("b.gcode"))];

        let first = run(&jobs, &card, &idle(), &|| {});
        assert_eq!(first.failed, None);
        assert_eq!(
            first.copied,
            [
                Copied { id: 1, file: "A.GCO".into(), fresh: true },
                Copied { id: 2, file: "B~2.GCO".into(), fresh: true },
            ]
        );
        assert_eq!(std::fs::read_to_string(card.join("b.gco")).unwrap(), "something else\n");
        assert_eq!(std::fs::read_to_string(card.join("B~2.GCO")).unwrap(), "G28\nG1 X2\n");

        let again = run(&jobs, &card, &idle(), &|| {});
        assert!(again.copied.iter().all(|c| !c.fresh));
        assert_eq!(again.copied[1].file, "B~2.GCO");
        assert!(!card.join("B~3.GCO").exists());
        assert!(
            std::fs::read_dir(&card)
                .unwrap()
                .all(|e| !file_name(&e.unwrap().path()).ends_with(".part"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn same_names_from_different_folders_do_not_overwrite() {
        let dir = scratch("same-name");
        let card = dir.join("card");
        std::fs::create_dir_all(&card).unwrap();
        for d in ["one", "two"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
            std::fs::write(dir.join(d).join("part.gcode"), format!("G28 ; {d}\n")).unwrap();
        }
        let jobs = vec![(1, dir.join("one/part.gcode")), (2, dir.join("two/part.gcode"))];
        let report = run(&jobs, &card, &idle(), &|| {});
        let files: Vec<&str> = report.copied.iter().map(|c| c.file.as_str()).collect();
        assert_eq!(files, ["PART.GCO", "PART~2.GCO"]);
        assert_eq!(std::fs::read_to_string(card.join("PART~2.GCO")).unwrap(), "G28 ; two\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_stops_before_anything_is_written() {
        let dir = scratch("missing");
        let card = dir.join("card");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(dir.join("a.gcode"), "G28\n").unwrap();
        let jobs = vec![(1, dir.join("a.gcode")), (2, dir.join("gone.gcode"))];
        let report = run(&jobs, &card, &idle(), &|| {});
        assert!(report.failed.unwrap().contains("gone.gcode"));
        assert!(report.copied.is_empty());
        assert_eq!(std::fs::read_dir(&card).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stopping_leaves_no_partial_file() {
        let dir = scratch("stop");
        let card = dir.join("card");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(dir.join("a.gcode"), vec![b'G'; 3 * CHUNK]).unwrap();
        let t = idle();
        t.stop();
        let report = run(&[(1, dir.join("a.gcode"))], &card, &t, &|| {});
        assert!(report.stopped);
        assert!(report.copied.is_empty());
        assert_eq!(std::fs::read_dir(&card).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spawned_copy_reports_progress_and_result() {
        let dir = scratch("spawn");
        let card = dir.join("card");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(dir.join("a.gcode"), vec![b'G'; 2 * CHUNK + 5]).unwrap();
        let t = Transfer::spawn(vec![(7, dir.join("a.gcode"))], card.clone(), Arc::new(|| {}));
        let report = loop {
            if let Some(r) = t.report() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(report.copied, [Copied { id: 7, file: "A.GCO".into(), fresh: true }]);
        let (done, total, current) = t.progress();
        assert_eq!((done, total), (2 * CHUNK as u64 + 5, 2 * CHUNK as u64 + 5));
        assert_eq!(current, "A.GCO");
        std::fs::remove_dir_all(&dir).ok();
    }
}
