//! Шаг 2: реализация трейта `Filesystem` поверх крейта `fatfs` (FAT12/16, RO).
//!
//! FUSE адресует узлы по inode, а `fatfs` — по путям. Здесь живёт мост:
//! реестр inode↔путь, который раздаёт стабильные inode по мере обхода дерева.
//! Читается всё заново от корня на каждый вызов — для дискеты это дёшево, а
//! кода минимум (кэш появится на шаге 3 вместе с медленным Визелем).

use std::collections::HashMap;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::block_source::BlockSource;
use crate::fs::{Attr, DirEntry, FileKind, Filesystem, VolStats};
use crate::volume_io::VolumeIo;

const ROOT_INO: u64 = 1;

/// Виртуальный пустой файл в корне тома. Наличие `.metadata_never_index`
/// говорит Spotlight не индексировать том — важно для медленного привода,
/// который иначе будет простаивать под фоновым обходом mds. Файла нет на FAT:
/// мы синтезируем его в getattr/lookup/readdir/read.
const NEVER_INDEX_INO: u64 = 2;
const NEVER_INDEX_NAME: &str = ".metadata_never_index";

struct Inodes {
    by_ino: HashMap<u64, String>,
    by_path: HashMap<String, u64>,
    next: u64,
}

pub struct FatFs<B: BlockSource> {
    fs: fatfs::FileSystem<VolumeIo<B>>,
    inodes: Mutex<Inodes>,
}

impl<B: BlockSource> FatFs<B> {
    pub fn open(src: B) -> io::Result<Self> {
        let vol = VolumeIo::new(src);
        let fs = fatfs::FileSystem::new(vol, fatfs::FsOptions::new())?;

        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(ROOT_INO, String::new());
        by_path.insert(String::new(), ROOT_INO);

        Ok(FatFs {
            fs,
            inodes: Mutex::new(Inodes {
                by_ino,
                by_path,
                // NEVER_INDEX_INO зарезервирован под виртуальный файл — стартуем после него.
                next: NEVER_INDEX_INO + 1,
            }),
        })
    }

    /// Выдать (или переиспользовать) стабильный inode для пути.
    fn intern(&self, path: &str) -> u64 {
        let mut g = self.inodes.lock().unwrap();
        if let Some(&ino) = g.by_path.get(path) {
            return ino;
        }
        let ino = g.next;
        g.next += 1;
        g.by_path.insert(path.to_string(), ino);
        g.by_ino.insert(ino, path.to_string());
        ino
    }

    fn path_of(&self, ino: u64) -> Option<String> {
        self.inodes.lock().unwrap().by_ino.get(&ino).cloned()
    }

    /// Атрибуты узла по пути ("" = корень).
    fn stat(&self, ino: u64, path: &str) -> Option<Attr> {
        if path.is_empty() {
            // У корня нет своей записи в каталоге — времена неизвестны.
            return Some(Attr {
                ino,
                size: 0,
                kind: FileKind::Dir,
                mtime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                atime: UNIX_EPOCH,
            });
        }
        let (parent, name) = split_parent(path);
        let root = self.fs.root_dir();
        let dir = if parent.is_empty() {
            root
        } else {
            root.open_dir(parent).ok()?
        };
        for entry in dir.iter() {
            let entry = entry.ok()?;
            if entry.file_name() == name {
                let kind = if entry.is_dir() {
                    FileKind::Dir
                } else {
                    FileKind::File
                };
                return Some(Attr {
                    ino,
                    size: entry.len(),
                    kind,
                    mtime: fat_datetime_to_system_time(entry.modified()),
                    crtime: fat_datetime_to_system_time(entry.created()),
                    atime: fat_date_to_system_time(entry.accessed()),
                });
            }
        }
        None
    }
}

/// Перевести FAT-время (локальное, без TZ) в `SystemTime` (UTC).
///
/// FAT хранит стенное время того часового пояса, где писали дискету. Finder
/// показывает время как `localtime(mtime)`. Чтобы в Finder читалось ровно то
/// время, что записано в FAT, трактуем поля как локальные и переводим в epoch
/// через `mktime` (учитывает текущий TZ/DST) — тогда обратный `localtime` в
/// Finder вернёт исходные цифры. Без этого поля из FAT «уезжали» бы на смещение
/// пояса (в MSK — на 3 часа, отсюда и «3 утра 1970» при нулевом времени).
fn fat_datetime_to_system_time(dt: fatfs::DateTime) -> SystemTime {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = dt.date.year as i32 - 1900;
    tm.tm_mon = dt.date.month as i32 - 1;
    tm.tm_mday = dt.date.day as i32;
    tm.tm_hour = dt.time.hour as i32;
    tm.tm_min = dt.time.min as i32;
    tm.tm_sec = dt.time.sec as i32;
    tm.tm_isdst = -1; // пусть libc сам определит летнее время
    let secs = unsafe { libc::mktime(&mut tm) };
    if secs < 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::from_secs(secs as u64) + Duration::from_millis(dt.time.millis as u64)
}

/// То же для FAT-даты доступа (там нет времени суток — берём полночь).
fn fat_date_to_system_time(d: fatfs::Date) -> SystemTime {
    fat_datetime_to_system_time(fatfs::DateTime {
        date: d,
        time: fatfs::Time {
            hour: 0,
            min: 0,
            sec: 0,
            millis: 0,
        },
    })
}

// SAFETY: FatFs используется только для чтения (RO). fatfs::FileSystem хранит
// &'static dyn OemCpConverter и &'static dyn TimeProvider — это stateless
// таблицы/функции без разделяемого мутабельного состояния, безопасные с любого
// потока. FatFs всегда доступен только под Mutex (в FuseAdapter), поэтому
// одновременного доступа нет. Без этого impl fatfs::FileSystem: !Send из-за
// ограничения крейта fatfs, не отражающего реальную небезопасность.
unsafe impl<B: BlockSource + Send> Send for FatFs<B> {}

impl<B: BlockSource> Filesystem for FatFs<B> {
    fn getattr(&self, ino: u64) -> Option<Attr> {
        if ino == NEVER_INDEX_INO {
            return Some(never_index_attr());
        }
        let path = self.path_of(ino)?;
        self.stat(ino, &path)
    }

    fn lookup(&self, parent: u64, name: &str) -> Option<Attr> {
        if parent == ROOT_INO && name == NEVER_INDEX_NAME {
            return Some(never_index_attr());
        }
        let parent_path = self.path_of(parent)?;
        let child = join(&parent_path, name);
        let ino = self.intern(&child);
        self.stat(ino, &child)
    }

    fn readdir(&self, ino: u64) -> Option<Vec<DirEntry>> {
        let path = self.path_of(ino)?;
        let root = self.fs.root_dir();
        let dir = if path.is_empty() {
            root
        } else {
            root.open_dir(&path).ok()?
        };

        let mut out = Vec::new();
        for entry in dir.iter() {
            let entry = entry.ok()?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            // Реального файла с таким именем на дискете быть не должно, но если
            // вдруг есть — пропускаем его в пользу виртуального (без дублей).
            if path.is_empty() && name == NEVER_INDEX_NAME {
                continue;
            }
            let child = join(&path, &name);
            let cino = self.intern(&child);
            out.push(DirEntry {
                ino: cino,
                name,
                kind: if entry.is_dir() {
                    FileKind::Dir
                } else {
                    FileKind::File
                },
            });
        }
        // Синтетический .metadata_never_index — только в корне.
        if path.is_empty() {
            out.push(DirEntry {
                ino: NEVER_INDEX_INO,
                name: NEVER_INDEX_NAME.to_string(),
                kind: FileKind::File,
            });
        }
        Some(out)
    }

    fn read(&self, ino: u64, offset: u64, size: u32) -> Option<Vec<u8>> {
        if ino == NEVER_INDEX_INO {
            // Пустой файл: читать нечего.
            return Some(Vec::new());
        }
        let Some(path) = self.path_of(ino) else {
            log::warn!("read: неизвестный inode {ino}");
            return None;
        };
        let root = self.fs.root_dir();
        let mut file = match root.open_file(&path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("read: open_file({path}) → {e}");
                return None;
            }
        };
        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            log::warn!("read: seek({offset}) в {path} → {e}");
            return None;
        }

        let mut buf = vec![0u8; size as usize];
        let mut filled = 0;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    log::warn!("read: {path} @ off {}: {e}", offset + filled as u64);
                    return None;
                }
            }
        }
        buf.truncate(filled);
        Some(buf)
    }

    fn stats(&self) -> Option<VolStats> {
        let s = self.fs.stats().ok()?;
        Some(VolStats {
            total_blocks: s.total_clusters() as u64,
            free_blocks: s.free_clusters() as u64,
            block_size: s.cluster_size(),
        })
    }
}

/// Атрибуты синтетического `.metadata_never_index` — пустой файл, времена нулевые.
fn never_index_attr() -> Attr {
    Attr {
        ino: NEVER_INDEX_INO,
        size: 0,
        kind: FileKind::File,
        mtime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        atime: UNIX_EPOCH,
    }
}

/// Разбить путь на (родитель, имя). "A/B/C" -> ("A/B", "C"); "C" -> ("", "C").
fn split_parent(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Стенное время из FAT должно вернуться теми же цифрами через `localtime`
    /// (именно так его показывает Finder), независимо от TZ тестовой машины.
    #[test]
    fn fat_time_roundtrips_through_localtime() {
        let dt = fatfs::DateTime {
            date: fatfs::Date {
                year: 1994,
                month: 3,
                day: 17,
            },
            time: fatfs::Time {
                hour: 9,
                min: 41,
                sec: 30,
                millis: 0,
            },
        };
        let st = fat_datetime_to_system_time(dt);
        let secs = st.duration_since(UNIX_EPOCH).unwrap().as_secs() as libc::time_t;

        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };

        assert_eq!(tm.tm_year + 1900, 1994);
        assert_eq!(tm.tm_mon + 1, 3);
        assert_eq!(tm.tm_mday, 17);
        assert_eq!(tm.tm_hour, 9);
        assert_eq!(tm.tm_min, 41);
        assert_eq!(tm.tm_sec, 30);
    }
}
