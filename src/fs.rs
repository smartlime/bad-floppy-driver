//! Шов №2: как толковать байты диска в дерево файлов.
//!
//! FUSE-адаптер общается ТОЛЬКО с этим трейтом. На шаге 2 его реализует
//! обёртка над крейтом `fatfs`; сейчас — `HelloFs` с синтетическим содержимым.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Dir,
    File,
}

/// Атрибуты одного узла (файла/каталога).
pub struct Attr {
    pub ino: u64,
    pub size: u64,
    pub kind: FileKind,
}

/// Одна запись каталога (без "." и ".." — их добавляет адаптер).
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub kind: FileKind,
}

/// Файловая система, представляемая в Finder. Всё read-only (решение №5).
pub trait Filesystem: Send {
    /// inode корневого каталога (в FUSE традиционно 1).
    fn root_ino(&self) -> u64 {
        1
    }
    fn getattr(&self, ino: u64) -> Option<Attr>;
    fn lookup(&self, parent: u64, name: &str) -> Option<Attr>;
    /// Дети каталога `ino` (без "." / "..").
    fn readdir(&self, ino: u64) -> Option<Vec<DirEntry>>;
    fn read(&self, ino: u64, offset: u64, size: u32) -> Option<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// Шаг 1: hello-FS — доказываем, что fuser ↔ macFUSE 5 работает на macOS 26.
// ---------------------------------------------------------------------------

const ROOT_INO: u64 = 1;
const README_INO: u64 = 2;

const README_NAME: &str = "README.txt";
const README_BODY: &str = "\
Этот том смонтирован программой floppy_mac (шаг 1: hello-FS).

Здесь нет реального файла на диске — байты, которые ты сейчас читаешь,
отдаёт мой Rust-код через macFUSE в ответ на FUSE-вызов read().

Следующий шаг: подставить сюда настоящую файловую систему (FAT12/16
через крейт fatfs), а источником байтов сделать образ .img, а затем
живую дискету через Greaseweazle.
";

pub struct HelloFs;

impl HelloFs {
    pub fn new() -> Self {
        HelloFs
    }
}

impl Filesystem for HelloFs {
    fn getattr(&self, ino: u64) -> Option<Attr> {
        match ino {
            ROOT_INO => Some(Attr {
                ino: ROOT_INO,
                size: 0,
                kind: FileKind::Dir,
            }),
            README_INO => Some(Attr {
                ino: README_INO,
                size: README_BODY.len() as u64,
                kind: FileKind::File,
            }),
            _ => None,
        }
    }

    fn lookup(&self, parent: u64, name: &str) -> Option<Attr> {
        if parent == ROOT_INO && name == README_NAME {
            self.getattr(README_INO)
        } else {
            None
        }
    }

    fn readdir(&self, ino: u64) -> Option<Vec<DirEntry>> {
        if ino != ROOT_INO {
            return None;
        }
        Some(vec![DirEntry {
            ino: README_INO,
            name: README_NAME.to_string(),
            kind: FileKind::File,
        }])
    }

    fn read(&self, ino: u64, offset: u64, size: u32) -> Option<Vec<u8>> {
        if ino != README_INO {
            return None;
        }
        let bytes = README_BODY.as_bytes();
        let start = (offset as usize).min(bytes.len());
        let end = (start + size as usize).min(bytes.len());
        Some(bytes[start..end].to_vec())
    }
}
