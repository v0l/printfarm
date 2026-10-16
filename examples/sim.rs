#[cfg(unix)]
fn main() {
    use serialport::SerialPort;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Duration;
    let (printer, host) = serialport::TTYPort::pair().expect("pty pair");
    let path = host.name().expect("pty name");
    let keep = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&path)
        .expect("pty");
    drop(host);
    let link = std::path::Path::new(
        &std::env::args().nth(1).unwrap_or_else(|| "/tmp/printfarm-sim".into()),
    )
    .to_path_buf();
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&path, &link).expect("link");
    println!("simulated Marlin on {} ({path})", link.display());
    let script = printfarm::printer::sim::Script {
        move_delay: Duration::from_millis(4),
        heat_rate: 4.0,
        sd_lines_per_s: 300.0,
        ..Default::default()
    };
    let _sim = printfarm::printer::sim::spawn(printer, script);
    loop {
        unsafe { libc::ioctl(keep.as_raw_fd(), libc::TIOCNXCL) };
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(not(unix))]
fn main() {}
