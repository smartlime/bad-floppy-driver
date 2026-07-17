//! Адаптер `BlockSource` (LBA-секторы) → линейный поток `Read + Seek + Write`,
//! который потребляет крейт `fatfs`. Запись запрещена (том read-only, решение №5).

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::block_source::BlockSource;

pub struct VolumeIo<B: BlockSource> {
    src: B,
    pos: u64,
    len: u64,
    bsize: u64,
}

impl<B: BlockSource> VolumeIo<B> {
    pub fn new(src: B) -> Self {
        let bsize = src.block_size() as u64;
        let len = src.block_count() * bsize;
        VolumeIo {
            src,
            pos: 0,
            len,
            bsize,
        }
    }
}

impl<B: BlockSource> Read for VolumeIo<B> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < buf.len() && self.pos < self.len {
            let lba = self.pos / self.bsize;
            let off = (self.pos % self.bsize) as usize;
            let block = self.src.read_block(lba)?;
            let avail = &block[off..];
            let n = avail
                .len()
                .min(buf.len() - written)
                .min((self.len - self.pos) as usize);
            buf[written..written + n].copy_from_slice(&avail[..n]);
            written += n;
            self.pos += n as u64;
        }
        Ok(written)
    }
}

impl<B: BlockSource> Seek for VolumeIo<B> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(n) => self.len as i128 + n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl<B: BlockSource> Write for VolumeIo<B> {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "volume is read-only",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
