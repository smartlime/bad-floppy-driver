//! Мост между нашим трейтом `Filesystem` и трейтом `fuser::Filesystem`.
//!
//! Здесь и только здесь живёт зависимость от FUSE-API. Всё остальное
//! (fs, block_source) о macFUSE ничего не знает.

use std::ffi::OsStr;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyXattr, Request,
};
use libc::{ENOENT, ENOTDIR};

use crate::fs::{Attr, FileKind, Filesystem};

/// TTL кэша метаданных в ядре. Для read-only тома можно щедро.
const TTL: Duration = Duration::from_secs(1);

pub struct FuseAdapter {
    inner: Box<dyn Filesystem>,
    uid: u32,
    gid: u32,
}

impl FuseAdapter {
    pub fn new(inner: Box<dyn Filesystem>) -> Self {
        // Владельцем узлов делаем текущего пользователя, чтобы Finder не ругался.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        FuseAdapter { inner, uid, gid }
    }

    fn to_file_attr(&self, a: &Attr) -> FileAttr {
        let (kind, perm, nlink) = match a.kind {
            FileKind::Dir => (FileType::Directory, 0o555, 2),
            FileKind::File => (FileType::RegularFile, 0o444, 1),
        };
        FileAttr {
            ino: a.ino,
            size: a.size,
            blocks: (a.size + 511) / 512,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }
}

impl fuser::Filesystem for FuseAdapter {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(ENOENT);
            return;
        };
        match self.inner.lookup(parent, name) {
            Some(attr) => reply.entry(&TTL, &self.to_file_attr(&attr), 0),
            None => reply.error(ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.inner.getattr(ino) {
            Some(attr) => reply.attr(&TTL, &self.to_file_attr(&attr)),
            None => reply.error(ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        // Read-only: разрешаем открытие, флагов handle не держим.
        reply.opened(0, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.inner.read(ino, offset.max(0) as u64, size) {
            Some(bytes) => reply.data(&bytes),
            None => reply.error(ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(children) = self.inner.readdir(ino) else {
            reply.error(ENOTDIR);
            return;
        };

        // "." и ".." добавляем сами; parent для корня = корень.
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ino, FileType::Directory, "..".to_string()),
        ];
        for e in children {
            let kind = match e.kind {
                FileKind::Dir => FileType::Directory,
                FileKind::File => FileType::RegularFile,
            };
            entries.push((e.ino, kind, e.name));
        }

        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            // reply.add вернёт true, когда буфер ядра заполнен.
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    // --- xattr: у нас их нет, но без корректных ответов cp/fcopyfile и Finder
    //     спотыкаются (это и вешало копирование). ---

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(libc::ENOATTR); // атрибута нет
    }

    fn listxattr(&mut self, _req: &Request<'_>, _ino: u64, size: u32, reply: ReplyXattr) {
        // Расширенных атрибутов нет: пустой список.
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        // Read-only том, доступ на чтение всем разрешаем.
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let (blocks, bfree, bsize) = match self.inner.stats() {
            Some(s) => (s.total_blocks, s.free_blocks, s.block_size.max(512)),
            None => (0, 0, 512),
        };
        // blocks, bfree, bavail, files, ffree, bsize, namelen, frsize
        reply.statfs(blocks, bfree, bfree, 0, 0, bsize, 255, bsize);
    }
}
