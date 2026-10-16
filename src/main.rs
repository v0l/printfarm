use clap::{Parser, Subcommand};
use printfarm::printer::marlin;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    files: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    Ports,
    Probe { port: String },
    Info { file: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => printfarm::app::run(cli.files).map_err(|e| e.to_string()),
        Some(Command::Ports) => {
            for p in marlin::ports() {
                println!("{}  {}", p.path, p.label);
            }
            Ok(())
        }
        Some(Command::Probe { port }) => marlin::detect(&port).map(|d| {
            println!("baud      {}", d.baud);
            println!("firmware  {}", d.firmware.unwrap_or_else(|| "-".into()));
            println!("machine   {}", d.machine.unwrap_or_else(|| "-".into()));
        }),
        Some(Command::Info { file }) => {
            printfarm::gcode::scan(&file).map_err(|e| format!("{}: {e}", file.display())).map(|m| {
                println!("commands  {}", m.commands);
                println!("layers    {}", m.layers);
                if let Some(e) = m.estimate() {
                    println!("estimate  {}", printfarm::app::hms(e));
                }
                if let Some(g) = m.filament_g {
                    println!("filament  {g:.2} g");
                }
                if let Some(s) = m.slicer {
                    println!("slicer    {s}");
                }
            })
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("printfarm: {e}");
            ExitCode::FAILURE
        }
    }
}
