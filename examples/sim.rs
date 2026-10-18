#[cfg(unix)]
fn main() {
    use printfarm::printer::sim::{Script, spawn_pty};
    use std::time::Duration;
    let link = std::path::PathBuf::from(
        std::env::args().nth(1).unwrap_or_else(|| "/tmp/printfarm-sim".into()),
    );
    let script = Script {
        move_delay: Duration::from_millis(4),
        heat_rate: 4.0,
        sd_lines_per_s: 300.0,
        ..Default::default()
    };
    let _sim = spawn_pty(&link, script).expect("simulated printer");
    println!("simulated Marlin on {}", link.display());
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(not(unix))]
fn main() {}
