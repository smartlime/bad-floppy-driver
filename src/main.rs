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
}

fn parse_args() -> Option<Args> {
    let mut a = Args {
        mountpoint: None,
        image: None,
        device: None,
        unit: 0,
        revs: 3,
        list_devices: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--image" => a.image = Some(PathBuf::from(it.next()?)),
            "--device" => a.device = Some(it.next()?),
            "--unit" => a.unit = it.next()?.parse().ok()?,
            "--revs" => a.revs = it.next()?.parse().ok()?,
            "--list-devices" => a.list_devices = true,
            _ => a.mountpoint = Some(arg),
        }
    }
    Some(a)
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
