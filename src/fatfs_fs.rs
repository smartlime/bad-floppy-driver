//! Шаг 2: реализация трейта `Filesystem` поверх крейта `fatfs` (FAT12/16, RO).
//!
//! FUSE адресует узлы по inode, а `fatfs` — по путям. Здесь живёт мост:
//! реестр inode↔путь, который раздаёт стабильные inode по мере обхода дерева.
//! Читается всё заново от корня на каждый вызов — для дискеты это дёшево, а
//! кода минимум (кэш появится на шаге 3 вместе с медленным Визелем).

use std::collections::HashMap;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::sync::Mutex;

use crate::block_source::BlockSource;
use crate::fs::{Attr, DirEntry, FileKind, Filesystem, VolStats};
use crate::volume_io::VolumeIo;

const ROOT_INO: u64 = 1;

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
                next: ROOT_INO + 1,
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

    /// Тип и размер узла по пути ("" = корень).
    fn stat(&self, path: &str) -> Option<(FileKind, u64)> {
        if path.is_empty() {
            return Some((FileKind::Dir, 0));
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
                return Some((kind, entry.len()));
            }
        }
        None
    }
}

impl<B: BlockSource> Filesystem for FatFs<B> {
    fn getattr(&self, ino: u64) -> Option<Attr> {
        let path = self.path_of(ino)?;
        let (kind, size) = self.stat(&path)?;
        Some(Attr { ino, size, kind })
    }

    fn lookup(&self, parent: u64, name: &str) -> Option<Attr> {
        let parent_path = self.path_of(parent)?;
        let child = join(&parent_path, name);
        let (kind, size) = self.stat(&child)?;
        let ino = self.intern(&child);
        Some(Attr { ino, size, kind })
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
        Some(out)
    }

    fn read(&self, ino: u64, offset: u64, size: u32) -> Option<Vec<u8>> {
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
