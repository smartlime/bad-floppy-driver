//! floppy_mac — read-only macOS FUSE-драйвер для дискет через Greaseweazle.
//!
//! Шаг 1 (hello-FS): смонтировать том с одним синтетическим README.txt и
//! снять риск совместимости `fuser` ↔ macFUSE 5 на macOS 26.
//!
//!   Использование:  floppy_mac <точка-монтирования>
//!   Размонтировать: umount <точка-монтирования>   (или Ctrl-C)

mod block_source;
mod fs;
mod fuse_adapter;

use std::process::ExitCode;

use fuser::MountOption;

use crate::fs::HelloFs;
use crate::fuse_adapter::FuseAdapter;

fn main() -> ExitCode {
    env_logger::init();

    let Some(mountpoint) = std::env::args().nth(1) else {
        eprintln!("usage: floppy_mac <mountpoint>");
        return ExitCode::FAILURE;
    };

    let fs = FuseAdapter::new(Box::new(HelloFs::new()));

    let options = vec![
        MountOption::RO,
        MountOption::FSName("floppy_mac".to_string()),
        MountOption::CUSTOM("volname=Floppy".to_string()),
    ];

    println!("Монтирую hello-FS в {mountpoint} (Ctrl-C для размонтирования)…");
    match fuser::mount2(fs, &mountpoint, &options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ошибка монтирования: {e}");
            ExitCode::FAILURE
        }
    }
}
