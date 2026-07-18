//! floppy_mac — read-only macOS FUSE-драйвер для дискет через Greaseweazle.
//!
//!   Шаг 1: hello-FS (синтетический README.txt).
//!   Шаг 2: монтирование готового образа .img через крейт `fatfs`.
//!   Шаг 3: живая дискета через Greaseweazle (свой протокол + MFM-декодер).
//!
//!   Использование:
//!     floppy_mac <точка>                      # hello-FS
//!     floppy_mac <точка> --image f.img        # FAT12/16 из образа
//!     floppy_mac <точка> --device <порт>      # живая дискета
//!     floppy_mac --list-devices               # список последовательных портов
//!   Размонтировать: umount <точка>   (или Ctrl-C)

mod actor;
mod block_source;
mod fatfs_fs;
mod fs;
mod fuse_adapter;
#[allow(dead_code)] // Pin/get_pin/Shugart зарезервированы под резидентный демон (авто-детект)
mod gw;
mod greaseweazle_src;
mod image;
#[allow(dead_code)] // encode-часть нужна только тестам
mod mfm;
mod volume_io;

use std::path::PathBuf;
use std::process::ExitCode;

use fuser::MountOption;

use crate::fatfs_fs::FatFs;
use crate::fs::{Filesystem, HelloFs};
use crate::fuse_adapter::FuseAdapter;
use crate::greaseweazle_src::GreaseweazleSource;
use crate::image::ImageFile;

struct Args {
    mountpoint: Option<String>,
    image: Option<PathBuf>,
    device: Option<String>,
    unit: u8,
    revs: u16,
    list_devices: bool,
    probe: bool,
}

fn parse_args() -> Option<Args> {
    let mut a = Args {
        mountpoint: None,
        image: None,
        device: None,
        unit: 0,
        revs: 3,
        list_devices: false,
        probe: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--image" => a.image = Some(PathBuf::from(it.next()?)),
            "--device" => a.device = Some(it.next()?),
            "--unit" => a.unit = it.next()?.parse().ok()?,
            "--revs" => a.revs = it.next()?.parse().ok()?,
            "--list-devices" => a.list_devices = true,
            "--probe" => a.probe = true,
            _ => a.mountpoint = Some(arg),
        }
    }
    Some(a)
}

/// Диагностика живого чтения без FUSE: прошивка, дорожка 0, секторы, геометрия.
fn probe(port: &str, unit: u8, revs: u16) -> std::io::Result<()> {
    use crate::gw::{BusType, Greaseweazle};

    let mut gw = Greaseweazle::open(port)?;
    let info = gw.get_info()?;
    println!(
        "Прошивка: v{}.{}, sample_freq = {} Гц",
        info.major, info.minor, info.sample_freq
    );

    gw.set_bus_type(BusType::Ibmpc)?;
    gw.select(unit)?;
    gw.set_motor(unit, true)?;
    std::thread::sleep(std::time::Duration::from_millis(600)); // раскрутка мотора

    for (cyl, head) in [(0u16, 0u8), (0, 1)] {
        gw.seek(cyl as u8)?;
        gw.select_head(head)?;
        let flux = gw.read_flux(revs)?;
        let mut sorted = flux.clone();
        sorted.sort_unstable();
        let (min, med, max) = if sorted.is_empty() {
            (0, 0, 0)
        } else {
            (sorted[0], sorted[sorted.len() / 2], sorted[sorted.len() - 1])
        };
        let sectors = crate::mfm::decode_track(&flux);
        let good = sectors.iter().filter(|s| s.data_crc_ok).count();
        println!(
            "\nдорожка C{cyl} H{head}: флукс={} интервалов (тики min/med/max = {min}/{med}/{max}), \
             секторов декодировано={} (валидных по CRC={})",
            flux.len(),
            sectors.len(),
            good
        );
        let mut nums: Vec<u8> = sectors.iter().map(|s| s.sector).collect();
        nums.sort_unstable();
        nums.dedup();
        println!("  номера секторов: {nums:?}");
        for s in sectors.iter().filter(|s| s.sector == 1) {
            println!(
                "  R1: C{} H{} N{} idCRC={} dataCRC={}",
                s.cyl, s.head, s.size_code, s.id_crc_ok, s.data_crc_ok
            );
            if let Some(g) = crate::greaseweazle_src::parse_bpb(&s.data) {
                println!("  BPB: {g:?}");
            }
        }
    }

    gw.set_motor(unit, false)?;
    gw.deselect()?;
    Ok(())
}

fn main() -> ExitCode {
    env_logger::init();

    let Some(args) = parse_args() else {
        eprintln!("usage: floppy_mac <mountpoint> [--image <path> | --device <port>]");
        return ExitCode::FAILURE;
    };

    if args.list_devices {
        match gw::enumerate() {
            Ok(ports) => {
                println!("Последовательные порты:");
                for p in ports {
                    println!("  {p}");
                }
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("не перечислить порты: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if args.probe {
        let Some(port) = args.device.clone() else {
            eprintln!("--probe требует --device <port>");
            return ExitCode::FAILURE;
        };
        return match probe(&port, args.unit, args.revs) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("probe: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let Some(mountpoint) = args.mountpoint.clone() else {
        eprintln!("нужна точка монтирования");
        return ExitCode::FAILURE;
    };

    // Выбор реализации `Filesystem` по аргументам — верхние слои от неё не зависят.
    let (inner, what): (Box<dyn Filesystem>, &str) = match (&args.image, &args.device) {
        (Some(path), _) => {
            let src = match ImageFile::open(path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("не открыть образ {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            // Тот же путь конкурентности/кэша, что и для Визеля. spt=18 — гранула
            // кэша для 1.44МБ (для образа неважно для корректности).
            let src = actor::spawn(src, 18);
            match FatFs::open(src) {
                Ok(fs) => (Box::new(fs), "образ"),
                Err(e) => {
                    eprintln!("не разобрать FAT в {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            }
        }
        (None, Some(port)) => {
            let src = match GreaseweazleSource::open(port, args.unit, args.revs) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Greaseweazle на {port}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Гранулу кэша берём равной физическим секторам на дорожку из BPB.
            let spt = src.geometry().sectors_per_track as u64;
            let src = actor::spawn(src, spt);
            match FatFs::open(src) {
                Ok(fs) => (Box::new(fs), "дискета"),
                Err(e) => {
                    eprintln!("не разобрать FAT с дискеты: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        (None, None) => (Box::new(HelloFs::new()), "hello-FS"),
    };

    let options = vec![
        MountOption::RO,
        MountOption::FSName("floppy_mac".to_string()),
        MountOption::CUSTOM("volname=Floppy".to_string()),
    ];

    println!("Монтирую {what} в {mountpoint} (Ctrl-C для размонтирования)…");
    match fuser::mount2(FuseAdapter::new(inner), &mountpoint, &options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ошибка монтирования: {e}");
            ExitCode::FAILURE
        }
    }
}
