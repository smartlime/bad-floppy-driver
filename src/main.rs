//! floppy_mac — read-only macOS FUSE-драйвер для дискет через Greaseweazle.
//!
//!   Шаг 1: hello-FS (синтетический README.txt).
//!   Шаг 2: монтирование готового образа .img через крейт `fatfs`.
//!
//!   Использование:
//!     floppy_mac <точка-монтирования>                 # hello-FS
//!     floppy_mac <точка-монтирования> --image f.img   # FAT12/16 из образа
//!   Размонтировать: umount <точка-монтирования>   (или Ctrl-C)

mod actor;
mod block_source;
mod fatfs_fs;
mod fs;
mod fuse_adapter;
#[allow(dead_code)] // управление/чтение — задействуется в greaseweazle_src
mod gw;
mod image;
#[allow(dead_code)] // используется с шага 3 (greaseweazle_src)
mod mfm;
mod volume_io;

use std::path::PathBuf;
use std::process::ExitCode;

use fuser::MountOption;

use crate::fatfs_fs::FatFs;
use crate::fs::{Filesystem, HelloFs};
use crate::fuse_adapter::FuseAdapter;
use crate::image::ImageFile;

struct Args {
    mountpoint: String,
    image: Option<PathBuf>,
}

fn parse_args() -> Option<Args> {
    let mut mountpoint = None;
    let mut image = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--image" => image = Some(PathBuf::from(it.next()?)),
            _ => mountpoint = Some(arg),
        }
    }
    Some(Args {
        mountpoint: mountpoint?,
        image,
    })
}

fn main() -> ExitCode {
    env_logger::init();

    let Some(args) = parse_args() else {
        eprintln!("usage: floppy_mac <mountpoint> [--image <path>]");
        return ExitCode::FAILURE;
    };

    // Выбор реализации `Filesystem` по аргументам — верхние слои от неё не зависят.
    let inner: Box<dyn Filesystem> = match &args.image {
        Some(path) => {
            let src = match ImageFile::open(path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("не открыть образ {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            // Источник заворачиваем в актор с дорожечным кэшем. Для .img это
            // избыточно (файл и так быстрый), но так проверяем ту же схему
            // конкурентности/кэша, что понадобится Визелю на шаге 3.
            // spt=18 — гранула кэша (стандарт 1.44МБ); на шаге 3 уточнится из BPB.
            let src = actor::spawn(src, 18);
            match FatFs::open(src) {
                Ok(fs) => Box::new(fs),
                Err(e) => {
                    eprintln!("не разобрать FAT в {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            }
        }
        None => Box::new(HelloFs::new()),
    };

    let options = vec![
        MountOption::RO,
        MountOption::FSName("floppy_mac".to_string()),
        MountOption::CUSTOM("volname=Floppy".to_string()),
    ];

    let what = if args.image.is_some() { "образ" } else { "hello-FS" };
    println!(
        "Монтирую {what} в {} (Ctrl-C для размонтирования)…",
        args.mountpoint
    );
    match fuser::mount2(FuseAdapter::new(inner), &args.mountpoint, &options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ошибка монтирования: {e}");
            ExitCode::FAILURE
        }
    }
}
