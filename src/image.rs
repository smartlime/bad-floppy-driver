//! Шаг 2: `BlockSource` поверх готового образа .img (линейный LBA-файл).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::block_source::BlockSource;

pub struct ImageFile {
    file: File,
    block_size: usize,
    block_count: u64,
}

impl ImageFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        // Стандартный сектор PC-дискеты. Для экзотики уточнится из BPB на шаге 3.
        let block_size = 512usize;
        Ok(ImageFile {
            file,
            block_size,
            block_count: len / block_size as u64,
        })
    }
}

impl BlockSource for ImageFile {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&mut self, lba: u64) -> io::Result<Vec<u8>> {
        self.file
            .seek(SeekFrom::Start(lba * self.block_size as u64))?;
        let mut buf = vec![0u8; self.block_size];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }
}
