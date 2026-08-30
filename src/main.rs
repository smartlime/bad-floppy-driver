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
//!   Размонтировать: umount <точка>   (или Ctrl+c)

mod actor;
mod block_source;
mod fatfs_fs;
mod fs;
mod fuse_adapter;
#[allow(dead_code)] // Pin/get_pin/Shugart зарезервированы под резидентный демон (авто-детект)
mod gw;
mod greaseweazle_src;
mod image;
#[cfg(target_os = "macos")]
mod mac_mount;
#[allow(dead_code)] // encode-часть нужна только тестам
mod mfm;
mod trdos_fs;
mod volume_io;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fuser::{Config, MountOption};

use crate::fatfs_fs::FatFs;
use crate::fs::{FileKind, Filesystem, HelloFs};
use crate::fuse_adapter::FuseAdapter;
use crate::greaseweazle_src::{DiskKind, GreaseweazleSource};
use crate::image::ImageFile;
use crate::trdos_fs::TrDosFs;

use crate::gw::BusType;

struct Args {
    mountpoint: Option<String>,
    image: Option<PathBuf>,
    device: Option<String>,
    unit: u8,
    bus: BusType,
    hd: bool,
    revs: u16,
    step: Option<u8>,
    list_devices: bool,
    probe: bool,
    recover: bool,
    verify: Option<PathBuf>,
    verbose: bool,
    help: bool,
}

/// Разобрать номер привода и определить тип шины по его виду:
///   "A"/"a" → (0, Ibmpc), "B"/"b" → (1, Ibmpc), цифры → (n, Shugart).
fn parse_unit(s: &str) -> Option<(u8, BusType)> {
    match s {
        "A" | "a" => Some((0, BusType::Ibmpc)),
        "B" | "b" => Some((1, BusType::Ibmpc)),
        _ => s.parse::<u8>().ok().map(|n| (n, BusType::Shugart)),
    }
}

fn parse_args() -> Option<Args> {
    let mut a = Args {
        mountpoint: None,
        image: None,
        device: None,
        unit: 0,
        bus: BusType::Shugart, // GW F1 Plus работает по Shugart (для цифровых приводов 0-3)
        hd: false,
        revs: 3,
        step: None,
        list_devices: false,
        probe: false,
        recover: false,
        verify: None,
        verbose: false,
        help: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--image" => a.image = Some(PathBuf::from(it.next()?)),
            "--device" => a.device = Some(it.next()?),
            "--unit" => {
                let (unit, bus) = parse_unit(&it.next()?)?;
                a.unit = unit;
                a.bus = bus;
            }
            "--hd" => a.hd = true,
            "--revs" => a.revs = it.next()?.parse().ok()?,
            "--step" => a.step = Some(it.next()?.parse().ok()?),
            "--list-devices" => a.list_devices = true,
            "--probe" => a.probe = true,
            "--recover" => a.recover = true,
            "--verify" => a.verify = Some(PathBuf::from(it.next()?)),
            "-v" | "--verbose" => a.verbose = true,
            "-h" | "--help" | "-?" => a.help = true,
            _ => a.mountpoint = Some(arg),
        }
    }
    Some(a)
}

fn print_help() {
    let lines = [
        "floppy_mac — read-only macOS FUSE-драйвер дискет через Greaseweazle",
        "",
        "ИСПОЛЬЗОВАНИЕ:",
        "  floppy_mac <точка> [--device <порт> | --image <файл>] [опции]",
        "  floppy_mac --list-devices",
        "  floppy_mac --probe --device <порт> [опции]",
        "",
        "ОПЦИИ:",
        "  -h, --help, -?     Показать эту справку",
        "  --device <порт>    Живая дискета через Greaseweazle (serial port)",
        "  --image <файл>     Смонтировать образ .img",
        "  --list-devices     Показать доступные serial-порты и выйти",
        "  --probe            Диагностика без монтирования (нужен --device)",
        "  --recover          Читать bitrot-дискеты: битые сектора -> нули, отчёт в конце",
        "  --unit <n>         Привод: 0-3 (Shugart/GW F1 Plus) или A/B (IBM PC шина);",
        "                     A/B=0/1 по умолчанию; тип шины определяется автоматически",
        "  --hd               HD-режим: assert density-select (Shugart пин 2 low)",
        "                     Нужен для 5.25\" HD приводов (Mitsumi, Teac FD-55GFR и др.)",
        "  --revs <n>         Оборотов на дорожку (по умолчанию 3; больше = надёжнее)",
        "  --step <n>         Физический шаг: 1 (по умолчанию, авто) или 2 (двойной шаг).",
        "                     Двойной шаг нужен для 40-дорожечных 5.25\" DD-дискет (360К/180К)",
        "                     в HD-приводе 96TPI. Авто-детект работает в большинстве случаев.",
        "  --verify <dir>     Сравнить все файлы FS с файлами в <dir> (без монтирования).",
        "  -v, --verbose      Debug-лог: команды GW, PLL, секторы, BPB",
        "",
        "РАЗМОНТИРОВАТЬ:",
        "  umount <точка>  (или Ctrl+C)",
        "",
        "ПРИМЕРЫ:",
        "  floppy_mac /tmp/fd --device /dev/tty.usbmodemGW1 -v",
        "  floppy_mac /tmp/fd --device /dev/tty.usbmodemGW1 --unit 1 --hd -v",
        "  floppy_mac /tmp/fd --device /dev/tty.usbmodemGW1 --unit A",
        "  floppy_mac /tmp/fd --device /dev/tty.usbmodemGW1 --step 2   # 40-дорожечная",
        "  floppy_mac --probe --device /dev/tty.usbmodemGW1 --unit 0 --hd -v",
        "  floppy_mac /tmp/fd --image disk.img",
        "  floppy_mac --verify /Volumes/NO\\ NAME --image disk.img  # сверить образ с диском",
    ];
    for line in lines {
        println!("{line}");
    }
}

/// Диагностика живого чтения без FUSE: прошивка, дорожка 0, секторы, геометрия.
fn probe(port: &str, unit: u8, bus: BusType, hd: bool, revs: u16) -> std::io::Result<()> {
    use crate::gw::Greaseweazle;

    let mut gw = Greaseweazle::open(port)?;
    let info = gw.get_info()?;
    println!(
        "Прошивка: v{}.{}, sample_freq = {} Гц",
        info.major, info.minor, info.sample_freq
    );

    gw.set_bus_type(bus)?;
    gw.select(unit)?;
    gw.set_motor(unit, true)?;
    if hd {
        if let Err(e) = gw.set_pin(2, false) {
            println!("  set_pin(2, false) не поддерживается: {e}");
        } else {
            println!("  density-select: пин 2 = low (HD)");
        }
    }
    println!("  раскрутка мотора (800 мс)…");
    std::thread::sleep(std::time::Duration::from_millis(800)); // раскрутка мотора

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

/// Пройти по дереву файловой системы и сравнить каждый файл с эталонным файлом
/// в `ref_dir`. Возвращает `true`, если все файлы совпали.
///
/// Синтетический `.metadata_never_index` пропускается (его нет на реальном диске).
fn verify_against(fs: &dyn Filesystem, ref_dir: &Path) -> bool {
    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut missing = 0usize;

    // BFS: (относительный путь к каталогу, ino каталога)
    let mut queue: Vec<(PathBuf, u64)> = vec![(PathBuf::new(), 1)];
    while let Some((prefix, dir_ino)) = queue.pop() {
        let Some(entries) = fs.readdir(dir_ino) else {
            eprintln!("  warn: readdir({dir_ino}) вернул None");
            continue;
        };
        for e in entries {
            if e.name == ".metadata_never_index" {
                continue; // виртуальный файл, не на диске
            }
            let rel = prefix.join(&e.name);
            match e.kind {
                FileKind::Dir => queue.push((rel, e.ino)),
                FileKind::File => {
                    let size = fs.getattr(e.ino)
                        .map(|a| a.size)
                        .unwrap_or(0);
                    // Читаем весь файл за один раз (фактически — потоками через
                    // fatfs; для дискет (max ~1 МБ) это разумно).
                    let fs_data = fs.read(e.ino, 0, size.min(u32::MAX as u64) as u32)
                        .unwrap_or_default();

                    let ref_path = ref_dir.join(&rel);
                    match std::fs::read(&ref_path) {
                        Ok(ref_data) => {
                            if fs_data == ref_data {
                                println!("  OK   {}", rel.display());
                                ok += 1;
                            } else {
                                println!(" FAIL  {}: {} байт из FS, {} байт в эталоне",
                                    rel.display(), fs_data.len(), ref_data.len());
                                fail += 1;
                            }
                        }
                        Err(err) => {
                            println!(" ???   {} — не найден в эталоне: {err}",
                                rel.display());
                            missing += 1;
                        }
                    }
                }
            }
        }
    }
    println!("Итого: {ok} OK, {fail} несовпадений, {missing} отсутствует в эталоне");
    fail == 0 && missing == 0
}

fn main() -> ExitCode {
    let Some(args) = parse_args() else {
        eprintln!("usage: floppy_mac <mountpoint> [--image <path> | --device <port>] [-v]");
        return ExitCode::FAILURE;
    };

    // -v/--verbose → подробный лог всех этапов в stderr (info). Иначе — RUST_LOG
    // или тихо (warn). Логи гарантированно flush'атся построчно.
    if args.help {
        print_help();
        return ExitCode::SUCCESS;
    }

    let default_level = if args.verbose { "debug" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .format_timestamp_millis()
        .init();

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
        return match probe(&port, args.unit, args.bus, args.hd, args.revs) {
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
    // Ручка актора нужна, чтобы взвести слежение за извлечением и дождаться Drop
    // источника (гашение мотора) при выходе; для hello-FS актора нет.
    let mut actor_handle: Option<actor::ActorHandle> = None;
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
            let (src, h) = actor::spawn(src, 18);
            actor_handle = Some(h);
            match FatFs::open(src) {
                Ok(fs) => (Box::new(fs), "образ"),
                Err(e) => {
                    eprintln!("не разобрать FAT в {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            }
        }
        (None, Some(port)) => {
            let src = match GreaseweazleSource::open(port, args.unit, args.bus, args.hd, args.revs, args.recover, args.step) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Greaseweazle на {port}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let geom = src.geom;
            let disk_kind = src.disk_kind;
            let spt = geom.sectors_per_track as u64;
            let (src, h) = actor::spawn(src, spt);
            actor_handle = Some(h);
            match disk_kind {
                DiskKind::Fat => match FatFs::open(src) {
                    Ok(fs) => (Box::new(fs) as Box<dyn Filesystem>, "дискета FAT"),
                    Err(e) => {
                        eprintln!("не разобрать FAT с дискеты: {e}");
                        return ExitCode::FAILURE;
                    }
                },
                DiskKind::TrDos => match TrDosFs::open(src, geom) {
                    Ok(fs) => (Box::new(fs) as Box<dyn Filesystem>, "дискета TR-DOS"),
                    Err(e) => {
                        eprintln!("не разобрать TR-DOS с дискеты: {e}");
                        return ExitCode::FAILURE;
                    }
                },
            }
        }
        (None, None) => (Box::new(HelloFs::new()), "hello-FS"),
    };

    // --verify: сравнить все файлы FS с эталонной директорией, не монтируя.
    if let Some(ref_dir) = &args.verify {
        println!("Верификация: сравниваю с «{}»…", ref_dir.display());
        let success = verify_against(inner.as_ref(), ref_dir);
        drop(inner);
        if let Some(h) = actor_handle {
            h.join();
        }
        return if success { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }

    // Предчтение метаданных (решение 6d): прогреваем кэш дорожек корня
    // (boot+FAT+корневой каталог), чтобы первый просмотр в Finder был мгновенным.
    if args.image.is_some() || args.device.is_some() {
        let t = std::time::Instant::now();
        let n = inner.readdir(1).map(|v| v.len()).unwrap_or(0);
        println!(
            "Предчтение метаданных: корень — {n} записей за {:?}",
            t.elapsed()
        );
    }

    let mount_options = vec![
        MountOption::RO,
        MountOption::FSName("floppy_mac".to_string()),
        MountOption::CUSTOM("volname=Floppy".to_string()),
        MountOption::CUSTOM("local".to_string()),
        MountOption::CUSTOM("noappledouble".to_string()),
    ];

    #[cfg(target_os = "macos")]
    let (session, mount_handle) = {
        let opt_strings: Vec<String> = mount_options
            .iter()
            .map(mac_mount::option_str)
            .collect();

        let (fd, mh) = match mac_mount::mount(&mountpoint, &opt_strings) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("ошибка монтирования: {e}");
                return ExitCode::FAILURE;
            }
        };

        let mut cfg = Config::default();
        cfg.mount_options = mount_options;

        let session = match fuser::Session::from_fd(
            FuseAdapter::new(inner),
            fd,
            fuser::SessionACL::Owner,
            cfg,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ошибка создания FUSE-сессии: {e}");
                return ExitCode::FAILURE;
            }
        };
        (session, mh)
    };

    #[cfg(not(target_os = "macos"))]
    let session = {
        let mut cfg = Config::default();
        cfg.mount_options = mount_options;
        match fuser::Session::new(FuseAdapter::new(inner), Path::new(&mountpoint), &cfg) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ошибка монтирования: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    println!("✔ Смонтировано: {what} в {mountpoint}. Работаю — Ctrl+c для размонтирования.");

    // Слежение за физическим извлечением носителя — только для живого привода.
    #[cfg(target_os = "macos")]
    if args.device.is_some() {
        if let Some(h) = &actor_handle {
            // MountHandle нужно передать в замыкание; оборачиваем в Arc чтобы
            // Sync+Send + Drop отработал в любом потоке.
            let mh = std::sync::Arc::new(mount_handle);
            let mp = mountpoint.clone();
            h.watch_eject(
                Box::new(move || {
                    mh.umount();
                    // После umount() macFUSE пробудит Session::run(), которая завершится сама.
                    // Доп. umount через umount(1) не нужен — macFUSE делает это внутри.
                    drop(mp); // зафиксировать захват переменной
                }),
                mountpoint.clone(),
            );
        }
    }

    // run() потребляет Session; после возврата FS уже дропнута → клиент актора
    // закрыт → актор завершается → Drop источника гасит мотор.
    let run = session.run();
    if let Some(h) = actor_handle {
        h.join();
    }
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ошибка монтирования: {e}");
            ExitCode::FAILURE
        }
    }
}
