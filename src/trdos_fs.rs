//! TR-DOS (ZX Spectrum) read-only filesystem.
//!
//! Каталог: дорожка 0, секторы R=1..8 (LBA 0..7), 128 записей по 16 байт.
//! LBA файла = start_track × spt + start_sector (0-based sector).

use std::io;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::block_source::BlockSource;
use crate::fs::{Attr, DirEntry, FileKind, Filesystem, VolStats};
use crate::greaseweazle_src::Geometry;

const ROOT_INO: u64 = 1;
const NEVER_INDEX_INO: u64 = 2;
const NEVER_INDEX_NAME: &str = ".metadata_never_index";
const FILE_INO_BASE: u64 = 3;

const ENTRY_SIZE: usize = 16;
const DIR_SECTORS: u64 = 8;

#[derive(Debug)]
struct Entry {
    name: String,
    start_lba: u64,
    sector_count: u8,
    len_bytes: u32,
}

pub struct TrDosFs<B: BlockSource> {
    src: Mutex<B>,
    geom: Geometry,
    entries: Vec<Entry>,
}

impl<B: BlockSource> TrDosFs<B> {
    /// Прочитать каталог TR-DOS с дискеты и построить список файлов.
    pub fn open(src: B, geom: Geometry) -> io::Result<Self> {
        let mut src = src;
        let spt = geom.sectors_per_track as u64;

        let mut entries = Vec::new();
        'outer: for sec_lba in 0..DIR_SECTORS {
            let block = src.read_block(sec_lba)?;
            for chunk in block.chunks_exact(ENTRY_SIZE) {
                let first = chunk[0];
                if first == 0x00 {
                    break 'outer; // конец каталога
                }
                if first == 0x01 {
                    continue; // удалённый файл — пропускаем
                }

                // Имя: байты 0-7 (пробелы в конце обрезаем).
                let raw_name: String = chunk[0..8]
                    .iter()
                    .map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '_' })
                    .collect::<String>()
                    .trim_end()
                    .to_string();

                let type_char = chunk[8];
                let ext = if (0x21..0x7F).contains(&type_char) {
                    format!(".{}", type_char as char)
                } else {
                    String::new()
                };
                let name = format!("{raw_name}{ext}");

                // len_bytes: байты [11..12] — длина файла в байтах (LE u16).
                let len_bytes = u16::from_le_bytes([chunk[11], chunk[12]]) as u32;
                let sector_count = chunk[13];
                let start_sector = chunk[14] as u64; // 0-base → IDAM R = start_sector+1
                let start_track = chunk[15] as u64;
                let start_lba = start_track * spt + start_sector;

                log::debug!(
                    "TR-DOS dir: {:20} lba={:4} secs={:2} len={}",
                    name, start_lba, sector_count, len_bytes
                );
                entries.push(Entry { name, start_lba, sector_count, len_bytes });
            }
        }

        log::info!("TR-DOS: {} файлов, spt={}, cyls={}", entries.len(), spt, geom.cylinders);
        Ok(TrDosFs {
            src: Mutex::new(src),
            geom,
            entries,
        })
    }

    fn ino_for(idx: usize) -> u64 {
        FILE_INO_BASE + idx as u64
    }

    fn entry_for(&self, ino: u64) -> Option<&Entry> {
        if ino < FILE_INO_BASE {
            return None;
        }
        self.entries.get((ino - FILE_INO_BASE) as usize)
    }
}

impl<B: BlockSource> Filesystem for TrDosFs<B> {
    fn getattr(&self, ino: u64) -> Option<Attr> {
        match ino {
            ROOT_INO => Some(Attr {
                ino: ROOT_INO,
                size: 0,
                kind: FileKind::Dir,
                mtime: SystemTime::UNIX_EPOCH,
                crtime: SystemTime::UNIX_EPOCH,
                atime: SystemTime::UNIX_EPOCH,
            }),
            NEVER_INDEX_INO => Some(never_index_attr()),
            _ => {
                let e = self.entry_for(ino)?;
                Some(file_attr(ino, e.len_bytes as u64))
            }
        }
    }

    fn lookup(&self, parent: u64, name: &str) -> Option<Attr> {
        if parent != ROOT_INO {
            return None;
        }
        if name == NEVER_INDEX_NAME {
            return Some(never_index_attr());
        }
        let idx = self.entries.iter().position(|e| e.name == name)?;
        let e = &self.entries[idx];
        Some(file_attr(Self::ino_for(idx), e.len_bytes as u64))
    }

    fn readdir(&self, ino: u64) -> Option<Vec<DirEntry>> {
        if ino != ROOT_INO {
            return None;
        }
        let mut out: Vec<DirEntry> = self
            .entries
            .iter()
            .enumerate()
            .map(|(idx, e)| DirEntry {
                ino: Self::ino_for(idx),
                name: e.name.clone(),
                kind: FileKind::File,
            })
            .collect();
        out.push(DirEntry {
            ino: NEVER_INDEX_INO,
            name: NEVER_INDEX_NAME.to_string(),
            kind: FileKind::File,
        });
        Some(out)
    }

    fn read(&self, ino: u64, offset: u64, size: u32) -> Option<Vec<u8>> {
        if ino == NEVER_INDEX_INO {
            return Some(Vec::new());
        }
        let e = self.entry_for(ino)?;
        let file_size = e.len_bytes as u64;
        if offset >= file_size {
            return Some(Vec::new());
        }
        let to_read = (size as u64).min(file_size - offset) as usize;
        let bps = self.geom.bytes_per_sector as u64;

        let first_sec = (offset / bps) as u8;
        let mut in_sec_off = (offset % bps) as usize;

        let mut out = Vec::with_capacity(to_read);
        let mut sec_idx = first_sec;

        while out.len() < to_read {
            if sec_idx >= e.sector_count {
                break;
            }
            let lba = e.start_lba + sec_idx as u64;
            let block = {
                let mut guard = self.src.lock().unwrap();
                match guard.read_block(lba) {
                    Ok(b) => b,
                    Err(err) => {
                        log::warn!("TR-DOS read lba={lba}: {err}");
                        return None;
                    }
                }
            };
            let from_this = (to_read - out.len()).min(block.len() - in_sec_off);
            out.extend_from_slice(&block[in_sec_off..in_sec_off + from_this]);
            in_sec_off = 0;
            sec_idx += 1;
        }
        Some(out)
    }

    fn stats(&self) -> Option<VolStats> {
        Some(VolStats {
            total_blocks: self.geom.total_sectors,
            free_blocks: 0,
            block_size: self.geom.bytes_per_sector as u32,
        })
    }
}

fn file_attr(ino: u64, size: u64) -> Attr {
    Attr {
        ino,
        size,
        kind: FileKind::File,
        mtime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        atime: SystemTime::UNIX_EPOCH,
    }
}

fn never_index_attr() -> Attr {
    Attr {
        ino: NEVER_INDEX_INO,
        size: 0,
        kind: FileKind::File,
        mtime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        atime: SystemTime::UNIX_EPOCH,
    }
}
