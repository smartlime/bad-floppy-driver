//! Шов №1: откуда берутся байты диска.
//!
//! На шаге 1 (hello-FS) ещё не используется — трейт зафиксирован здесь заранее,
//! чтобы верхние слои сразу проектировались против него.
//!
//!   - Шаг 2: `ImageFile` (mmap готового .img)
//!   - Шаг 3: `Greaseweazle` (живая дискета, в акторе, с дорожечным кэшем)
//!   - Фаза 4: собственный протокол + MFM-декодер (замена только этой реализации)

use std::io;

/// Логическая геометрия носителя. Заполняется из BPB (BPB-first, решение №7).
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors_per_track: u8,
    pub bytes_per_sector: u16,
}

/// Источник блоков. Наружу — посекторный доступ (решение №6);
/// дорожечное чтение и кэш прячутся в конкретной реализации (актор на шаге 3).
pub trait BlockSource: Send {
    fn geometry(&self) -> Geometry;

    /// Прочитать один сектор. `cyl`/`head`/`sector` — физическая адресация
    /// (sector нумеруется с 1, как в CHS/IDAM).
    fn read_sector(&mut self, cyl: u16, head: u8, sector: u8) -> io::Result<Vec<u8>>;
}
