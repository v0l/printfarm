#[cfg(unix)]
fn main() {
    use serialport::SerialPort;
    use std::time::Duration;
    let (printer, host) = serialport::TTYPort::pair().expect("pty pair");
    let path = host.name().expect("pty name");
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
        ..Default::default()
    };
    let _sim = printfarm::printer::sim::spawn(printer, script);
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(not(unix))]
fn main() {}
