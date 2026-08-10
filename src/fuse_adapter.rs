//! Мост между нашим трейтом `Filesystem` и трейтом `fuser::Filesystem`.
//!
//! Здесь и только здесь живёт зависимость от FUSE-API. Всё остальное
//! (fs, block_source) о macFUSE ничего не знает.

use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::Duration;

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo,
    LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyXattr, Request,
};

use crate::fs::{Attr, FileKind, Filesystem};

/// TTL кэша метаданных в ядре. Для read-only тома можно щедро.
const TTL: Duration = Duration::from_secs(1);

/// `Mutex` делает `FuseAdapter: Sync` даже если внутренняя FS (например, fatfs)
/// использует `RefCell` и не реализует `Sync` сама по себе.
pub struct FuseAdapter {
    inner: Mutex<Box<dyn Filesystem>>,
    uid: u32,
    gid: u32,
}

impl FuseAdapter {
    pub fn new(inner: Box<dyn Filesystem>) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        FuseAdapter { inner: Mutex::new(inner), uid, gid }
    }

    fn to_file_attr(&self, a: &Attr) -> FileAttr {
        let (kind, perm, nlink) = match a.kind {
            FileKind::Dir => (FileType::Directory, 0o555, 2),
            FileKind::File => (FileType::RegularFile, 0o444, 1),
        };
        FileAttr {
            ino: INodeNo(a.ino),
            size: a.size,
            blocks: (a.size + 511) / 512,
            atime: a.atime,
            mtime: a.mtime,
            ctime: a.mtime,
            crtime: a.crtime,
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
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        let inner = self.inner.lock().unwrap();
        match inner.lookup(parent.0, name_str) {
            Some(attr) => reply.entry(&TTL, &self.to_file_attr(&attr), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let inner = self.inner.lock().unwrap();
        match inner.getattr(ino.0) {
            Some(attr) => reply.attr(&TTL, &self.to_file_attr(&attr)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let inner = self.inner.lock().unwrap();
        match inner.read(ino.0, offset, size) {
            Some(bytes) => reply.data(&bytes),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let inner = self.inner.lock().unwrap();
        let Some(children) = inner.readdir(ino.0) else {
            reply.error(Errno::ENOTDIR);
            return;
        };

        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (ino.0, FileType::Directory, "..".to_string()),
        ];
        for e in children {
            let kind = match e.kind {
                FileKind::Dir => FileType::Directory,
                FileKind::File => FileType::RegularFile,
            };
            entries.push((e.ino, kind, e.name));
        }
        drop(inner); // освобождаем мьютекс перед итерацией буфера

        for (i, (entry_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(entry_ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENOATTR);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let inner = self.inner.lock().unwrap();
        let (blocks, bfree, bsize) = match inner.stats() {
            Some(s) => (s.total_blocks, s.free_blocks, s.block_size.max(512)),
            None => (0, 0, 512),
        };
        reply.statfs(blocks, bfree, bfree, 0, 0, bsize, 255, bsize);
    }
}
