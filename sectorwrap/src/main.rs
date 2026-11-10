use clap::{Parser, Subcommand};
use sectorwrap_core::sgio::SgIoBackend;
use sectorwrap_core::Geometry;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const STATE_DIR: &str = "/run/sectorwrap";
const STATE_FILE: &str = "/run/sectorwrap/mappings";

#[derive(Parser, Debug)]
#[command(
    name = "sectorwrap",
    about = "Manage protected ublk mappings for SCSI disks"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandLine,
}

#[derive(Subcommand, Debug)]
enum CommandLine {
    Status,
    Format {
        #[arg(long)]
        yes: bool,
        #[arg(long, help = "Only format this many logical sectors")]
        sectors: Option<u64>,
        devices: Vec<PathBuf>,
    },
    Start {
        #[arg(long, help = "Expose at most this many logical sectors")]
        sectors: Option<u64>,
        devices: Vec<PathBuf>,
    },
    Stop {
        device: PathBuf,
    },
}

#[derive(Debug, Clone)]
struct Mapping {
    sg: String,
    ublk: String,
    id: i32,
    pid: u32,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        CommandLine::Status => status(),
        CommandLine::Format {
            yes,
            sectors,
            devices,
        } => format_devices(yes, sectors, &devices),
        CommandLine::Start { sectors, devices } => start_devices(sectors, &devices),
        CommandLine::Stop { device } => stop_device(&device),
    };
    if let Err(error) = result {
        eprintln!("sectorwrap: {error}");
        std::process::exit(1);
    }
}

fn format_devices(
    yes: bool,
    sector_limit: Option<u64>,
    devices: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    if devices.is_empty() {
        return Err("format requires at least one /dev/sgX device".into());
    }
    if sector_limit == Some(0) {
        return Err("--sectors must be greater than zero".into());
    }
    let mut plans = Vec::new();
    for path in devices {
        let mut probe = SgIoBackend::open(
            path.to_str().ok_or("invalid device path")?,
            Geometry::for_sector_size(528)?,
        )
        .map_err(|e| format!("{path:?}: {e}"))?;
        let (capacity, block_size) = probe.capacity().map_err(|e| format!("{path:?}: {e}"))?;
        if block_size == 512 {
            return Err(format!("{path:?}: native 512-byte media must not be formatted").into());
        }
        let geometry = Geometry::for_sector_size(block_size)?;
        let sectors = sector_limit.unwrap_or(capacity);
        if sectors > capacity {
            return Err(format!("{path:?}: --sectors exceeds the device capacity").into());
        }
        plans.push((path, sectors, block_size, geometry));
    }
    println!("! WARNING POTENTIAL DATA LOSS !");
    println!("Formatting Devices");
    for (path, _, _, _) in &plans {
        println!("   - {}", path.display());
    }
    println!(
        "The first {} logical sectors of every device will be overwritten.",
        sector_limit.map_or_else(|| "all".to_owned(), |value| value.to_string())
    );
    if !yes {
        print!("Confirm? (y/N) ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }
    for (path, sectors, block_size, geometry) in plans {
        ensure_not_mapped(path)?;
        println!(
            "Formatting {}: {} sectors x {} bytes, payload {} bytes, protection {:?}",
            path.display(),
            sectors,
            block_size,
            geometry.payload_bytes,
            geometry.protection
        );
        let mut backend = SgIoBackend::open(path.to_str().ok_or("invalid device path")?, geometry)?;
        backend.capacity()?;
        backend.format(sectors)?;
        println!("Format completed: {}", path.display());
    }
    Ok(())
}

fn start_devices(
    sector_limit: Option<u64>,
    devices: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    if devices.is_empty() {
        return Err("start requires at least one /dev/sgX device".into());
    }
    if sector_limit == Some(0) {
        return Err("--sectors must be greater than zero".into());
    }
    fs::create_dir_all(STATE_DIR)?;
    let mut mappings = read_mappings()?;
    let worker = std::env::current_exe()?.with_file_name("sectorwrap-ublk");
    for path in devices {
        let sg = path.to_str().ok_or("invalid device path")?.to_owned();
        if mappings.iter().any(|mapping| mapping.sg == sg) {
            return Err(format!("{sg} is already mapped").into());
        }
        let mut probe = SgIoBackend::open(&sg, Geometry::for_sector_size(528)?)?;
        let (capacity, block_size) = probe.capacity()?;
        let geometry = if block_size == 512 {
            None
        } else {
            Some(Geometry::for_sector_size(block_size)?)
        };
        let sectors = sector_limit.unwrap_or(capacity);
        if sectors > capacity {
            return Err(format!("{sg}: --sectors exceeds the device capacity").into());
        }
        let sector_size = geometry.map(|g| g.physical_bytes).unwrap_or(512);
        let id = next_id(&mappings);
        let mut command = Command::new(&worker);
        command
            .arg("--device")
            .arg(&sg)
            .arg("--sector-size")
            .arg(sector_size.to_string())
            .arg("--id")
            .arg(id.to_string())
            .arg("--queues")
            .arg("1")
            .arg("--depth")
            .arg("64");
        if let Some(sector_limit) = sector_limit {
            command.arg("--sectors").arg(sector_limit.to_string());
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("start {sg}: {e}"))?;
        let ublk = format!("/dev/ublkb{id}");
        for _ in 0..100 {
            if Path::new(&ublk).exists() {
                break;
            }
            if !Path::new(&format!("/proc/{}", child.id())).exists() {
                return Err(format!("worker for {sg} exited before creating {ublk}").into());
            }
            thread::sleep(Duration::from_millis(100));
        }
        if !Path::new(&ublk).exists() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            return Err(format!("timed out waiting for {ublk}").into());
        }
        mappings.push(Mapping {
            sg: sg.clone(),
            ublk: ublk.clone(),
            id,
            pid: child.id(),
        });
        println!("{sg} -> {ublk}");
    }
    write_mappings(&mappings)?;
    Ok(())
}

fn stop_device(device: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let wanted = device.to_string_lossy();
    let mut mappings = read_mappings()?;
    let index = mappings
        .iter()
        .position(|mapping| mapping.ublk == wanted)
        .ok_or_else(|| format!("{wanted} is not managed by sectorwrap"))?;
    let mapping = mappings[index].clone();
    unsafe {
        libc::kill(mapping.pid as i32, libc::SIGTERM);
    }
    for _ in 0..100 {
        if !Path::new(&format!("/proc/{}", mapping.pid)).exists() {
            mappings.remove(index);
            write_mappings(&mappings)?;
            println!("Stopped {}. {} is IDLE.", mapping.ublk, mapping.sg);
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("worker {} did not stop; mapping was kept", mapping.pid).into())
}

fn status() -> Result<(), Box<dyn std::error::Error>> {
    println!("SCSI devices:");
    let mut entries = fs::read_dir("/dev")?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("sg"))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let mut probe = match SgIoBackend::open(
            path.to_str().unwrap_or_default(),
            Geometry::for_sector_size(528)?,
        ) {
            Ok(value) => value,
            Err(error) => {
                println!("  {}  unavailable ({error})", path.display());
                continue;
            }
        };
        match probe.capacity() {
            Ok((sectors, block_size)) => println!(
                "  {}  {} B  {} sectors  {}",
                path.display(),
                block_size,
                sectors,
                capacity_string(sectors, block_size)
            ),
            Err(error) => println!("  {}  probe failed ({error})", path.display()),
        }
    }
    println!("\nActive mappings:");
    for mapping in read_mappings()? {
        let alive = Path::new(&format!("/proc/{}", mapping.pid)).exists();
        println!(
            "  {} -> {}  pid={}  {}",
            mapping.sg,
            mapping.ublk,
            mapping.pid,
            if alive { "running" } else { "stale" }
        );
    }
    Ok(())
}

fn capacity_string(sectors: u64, bytes: usize) -> String {
    let total = sectors.saturating_mul(bytes as u64);
    format!("{total} bytes ({:.1} GiB)", total as f64 / 1_073_741_824.0)
}

fn ensure_not_mapped(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if read_mappings()?
        .iter()
        .any(|mapping| mapping.sg == path.to_string_lossy())
    {
        return Err(format!("{} is currently mapped; stop it first", path.display()).into());
    }
    Ok(())
}

fn next_id(mappings: &[Mapping]) -> i32 {
    (0..1024)
        .find(|id| mappings.iter().all(|mapping| mapping.id != *id))
        .unwrap_or(0)
}

fn read_mappings() -> Result<Vec<Mapping>, Box<dyn std::error::Error>> {
    let file = match OpenOptions::new().read(true).open(STATE_FILE) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut result = Vec::new();
    for line in io::BufReader::new(file).lines() {
        let fields = line?.split('\t').map(str::to_owned).collect::<Vec<_>>();
        if fields.len() == 4 {
            result.push(Mapping {
                sg: fields[0].clone(),
                ublk: fields[1].clone(),
                id: fields[2].parse()?,
                pid: fields[3].parse()?,
            });
        }
    }
    Ok(result)
}

fn write_mappings(mappings: &[Mapping]) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(STATE_DIR)?;
    let temporary = format!("{STATE_FILE}.tmp.{}", std::process::id());
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    for mapping in mappings {
        writeln!(
            file,
            "{}\t{}\t{}\t{}",
            mapping.sg, mapping.ublk, mapping.id, mapping.pid
        )?;
    }
    file.sync_all()?;
    fs::rename(temporary, STATE_FILE)?;
    Ok(())
}
