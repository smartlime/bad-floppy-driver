//! Монтирование FUSE на macOS через macFUSE 5.x.
//!
//! fuser 0.18 использует `fuse_mount_compat25` (API macFUSE 4.x), который
//! возвращает ENOTTY на macFUSE 5.x. Этот модуль вызывает новый `fuse_mount`
//! (API версии 26, FUSE_USE_VERSION >= 26) напрямую, а затем передаёт
//! полученный fd в `Session::from_fd`.

use std::ffi::{CString, c_char, c_int};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

/// Непрозрачный тип fuse_chan из libfuse.
#[repr(C)]
pub struct FuseChan {
    _private: [u8; 0],
}

#[repr(C)]
struct FuseArgs {
    argc: c_int,
    argv: *const *const c_char,
    allocated: c_int,
}

extern "C" {
    // FUSE_USE_VERSION >= 26 API — работает на macFUSE 5.x.
    fn fuse_mount(mountpoint: *const c_char, args: *const FuseArgs) -> *mut FuseChan;
    fn fuse_chan_fd(ch: *mut FuseChan) -> c_int;
    fn fuse_unmount(mountpoint: *const c_char, ch: *mut FuseChan);
}

/// Ручка смонтированного FUSE-тома. Размонтирует при Drop.
pub struct MountHandle {
    mountpoint: CString,
    chan: *mut FuseChan,
    unmounted: AtomicBool,
}

// Safety: chan является указателем на объект macFUSE, защищённым от
// concurrent access самим macFUSE (кернел-объект), доступ к umount
// сериализован через AtomicBool.
unsafe impl Send for MountHandle {}
unsafe impl Sync for MountHandle {}

impl MountHandle {
    /// Размонтировать том (идемпотентно).
    pub fn umount(&self) {
        if self.unmounted.swap(true, Ordering::SeqCst) {
            return;
        }
        unsafe { fuse_unmount(self.mountpoint.as_ptr(), self.chan) };
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        self.umount();
    }
}

/// Смонтировать FUSE-том через `fuse_mount` (macFUSE 5.x API).
///
/// Возвращает `OwnedFd` для передачи в `Session::from_fd` и `MountHandle`,
/// который автоматически размонтирует при drop'е.
///
/// `options` — строки опций монтирования (аргументы после `-o`, без самого `-o`).
pub fn mount(mountpoint: &str, options: &[String]) -> io::Result<(OwnedFd, MountHandle)> {
    let mountpoint_c = CString::new(mountpoint)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // Строим argv: ["floppy_mac", "-o", "opt1", "-o", "opt2", ...]
    let mut argv_cs: Vec<CString> = vec![CString::new("floppy_mac").unwrap()];
    for opt in options {
        argv_cs.push(CString::new("-o").unwrap());
        argv_cs.push(
            CString::new(opt.as_str())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
        );
    }
    let argv_ptrs: Vec<*const c_char> = argv_cs.iter().map(|s| s.as_ptr()).collect();
    let args = FuseArgs {
        argc: argv_ptrs.len() as c_int,
        argv: argv_ptrs.as_ptr(),
        allocated: 0,
    };

    let chan = unsafe { fuse_mount(mountpoint_c.as_ptr(), &args) };
    if chan.is_null() {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            if err.raw_os_error() == Some(0) {
                io::ErrorKind::Other
            } else {
                err.kind()
            },
            format!("fuse_mount: {err}"),
        ));
    }

    // fuse_chan_fd возвращает fd, принадлежащий chan. Дублируем, чтобы Session
    // мог владеть своей копией независимо от жизни MountHandle.
    let raw_fd = unsafe { fuse_chan_fd(chan) };
    let dup_fd = unsafe { libc::dup(raw_fd) };
    if dup_fd < 0 {
        unsafe { fuse_unmount(mountpoint_c.as_ptr(), chan) };
        return Err(io::Error::last_os_error());
    }
    let owned_fd = unsafe { OwnedFd::from_raw_fd(dup_fd) };

    Ok((
        owned_fd,
        MountHandle {
            mountpoint: mountpoint_c,
            chan,
            unmounted: AtomicBool::new(false),
        },
    ))
}

/// Конвертировать `MountOption` в строку для `-o` аргумента fuse_mount.
pub fn option_str(opt: &fuser::MountOption) -> String {
    match opt {
        fuser::MountOption::FSName(name) => format!("fsname={name}"),
        fuser::MountOption::Subtype(st) => format!("subtype={st}"),
        fuser::MountOption::CUSTOM(v) => v.clone(),
        fuser::MountOption::AutoUnmount => "auto_unmount".to_string(),
        fuser::MountOption::DefaultPermissions => "default_permissions".to_string(),
        fuser::MountOption::Dev => "dev".to_string(),
        fuser::MountOption::NoDev => "nodev".to_string(),
        fuser::MountOption::Suid => "suid".to_string(),
        fuser::MountOption::NoSuid => "nosuid".to_string(),
        fuser::MountOption::RO => "ro".to_string(),
        fuser::MountOption::RW => "rw".to_string(),
        fuser::MountOption::Exec => "exec".to_string(),
        fuser::MountOption::NoExec => "noexec".to_string(),
        fuser::MountOption::Atime => "atime".to_string(),
        fuser::MountOption::NoAtime => "noatime".to_string(),
        fuser::MountOption::DirSync => "dirsync".to_string(),
        fuser::MountOption::Sync => "sync".to_string(),
        fuser::MountOption::Async => "async".to_string(),
    }
}
