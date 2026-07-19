//! Шаг 3: `BlockSource` поверх живой дискеты через Greaseweazle.
//!
//! Геометрию берём из BPB дорожки 0 (решение №7 «BPB-first»). LBA→CHS считаем
//! из геометрии, читаем флукс дорожки, декодируем MFM, отдаём сектор. Битый/
//! пропавший сектор → EIO (решение №9, режим по умолчанию).
//!
//! Тонкость производительности: актор (кэш дорожек) просит секторы по одному и
//! на промах дорожки вызовет `read_block` `spt` раз. Чтобы это не превращалось
//! в `spt` физических чтений флукса, здесь держим кэш ПОСЛЕДНЕЙ прочитанной
//! дорожки: первый сектор дорожки читает флукс, остальные берутся из него.

use std::collections::HashMap;
use std::io;

use crate::block_source::BlockSource;
use crate::gw::{BusType, Greaseweazle};
use crate::mfm;

#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors_per_track: u8,
    pub bytes_per_sector: u16,
    pub total_sectors: u64,
}

/// Разобрать геометрию из BPB загрузочного сектора (первые 512 байт тома).
pub fn parse_bpb(boot: &[u8]) -> Option<Geometry> {
    if boot.len() < 36 {
        return None;
    }
    let u16le = |o: usize| u16::from_le_bytes([boot[o], boot[o + 1]]);
    let u32le = |o: usize| u32::from_le_bytes([boot[o], boot[o + 1], boot[o + 2], boot[o + 3]]);

    let bytes_per_sector = u16le(0x0B);
    let sectors_per_track = u16le(0x18);
    let heads = u16le(0x1A);
    let total16 = u16le(0x13);
    let total = if total16 != 0 {
        total16 as u64
    } else {
        u32le(0x20) as u64
    };

    // Санити: PC-дискеты.
    if bytes_per_sector == 0 || sectors_per_track == 0 || heads == 0 || total == 0 {
        return None;
    }
    let spc = (sectors_per_track as u64) * (heads as u64);
    let cylinders = ((total + spc - 1) / spc) as u16;

    Some(Geometry {
        cylinders,
        heads: heads as u8,
        sectors_per_track: sectors_per_track as u8,
        bytes_per_sector,
        total_sectors: total,
    })
}

/// LBA → (цилиндр, головка, номер сектора с 1).
pub fn lba_to_chs(g: &Geometry, lba: u64) -> (u16, u8, u8) {
    let spt = g.sectors_per_track as u64;
    let per_cyl = spt * g.heads as u64;
    let cyl = (lba / per_cyl) as u16;
    let rem = lba % per_cyl;
    let head = (rem / spt) as u8;
    let sector = (rem % spt) as u8 + 1;
    (cyl, head, sector)
}

pub struct GreaseweazleSource {
    gw: Greaseweazle,
    geom: Geometry,
    unit: u8,
    revs: u16,
    cur_track: Option<(u16, u8)>,
    cur_sectors: HashMap<u8, mfm::Sector>, // R → лучший (по CRC) сектор дорожки
}

impl GreaseweazleSource {
    /// Открыть привод, раскрутить мотор, определить геометрию по BPB дорожки 0.
    pub fn open(port: &str, unit: u8, revs: u16) -> io::Result<Self> {
        let mut gw = Greaseweazle::open(port)?;
        gw.set_bus_type(BusType::Ibmpc)?;
        gw.select(unit)?;
        gw.set_motor(unit, true)?;

        // Прочитать дорожку 0 головки 0 и найти boot-сектор (R=1).
        gw.seek(0)?;
        gw.select_head(0)?;
        let flux = gw.read_flux(revs)?;
        let sectors = mfm::decode_track(&flux);
        let boot = sectors
            .iter()
            .find(|s| s.sector == 1 && s.data_crc_ok)
            .or_else(|| sectors.iter().find(|s| s.sector == 1))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "boot-сектор не найден"))?;
        let geom = parse_bpb(&boot.data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "BPB не распознан"))?;

        log::info!(
            "геометрия: {} cyl × {} hd × {} spt × {} B = {} секторов",
            geom.cylinders,
            geom.heads,
            geom.sectors_per_track,
            geom.bytes_per_sector,
            geom.total_sectors
        );

        Ok(GreaseweazleSource {
            gw,
            geom,
            unit,
            revs,
            cur_track: None,
            cur_sectors: HashMap::new(),
        })
    }

    pub fn geometry(&self) -> Geometry {
        self.geom
    }

    /// Физически прочитать дорожку за `revs` оборотов и слить удачные (по CRC)
    /// секторы в кэш текущей дорожки. `fresh` очищает кэш (новая дорожка).
    fn read_track_into_cache(
        &mut self,
        cyl: u16,
        head: u8,
        revs: u16,
        fresh: bool,
    ) -> io::Result<()> {
        // Прошивка снимает выбор привода после простоя (иначе seek → NoUnit),
        // поэтому пере-подтверждаем выбор и мотор перед каждым физическим чтением.
        self.gw.select(self.unit)?;
        self.gw.set_motor(self.unit, true)?;
        self.gw.seek(cyl as u8)?;
        self.gw.select_head(head)?;
        let flux = self.gw.read_flux(revs)?;
        let sectors = mfm::decode_track(&flux);

        if fresh {
            self.cur_sectors.clear();
            self.cur_track = Some((cyl, head));
        }
        for s in sectors {
            // Сливаем: сектор с валидным CRC вытесняет отсутствующий/битый.
            let better = match self.cur_sectors.get(&s.sector) {
                Some(existing) => !existing.data_crc_ok && s.data_crc_ok,
                None => true,
            };
            if better {
                self.cur_sectors.insert(s.sector, s);
            }
        }
        Ok(())
    }

    fn have_good(&self, sec: u8) -> bool {
        self.cur_sectors.get(&sec).is_some_and(|s| s.data_crc_ok)
    }
}

impl BlockSource for GreaseweazleSource {
    fn block_size(&self) -> usize {
        self.geom.bytes_per_sector as usize
    }

    fn block_count(&self) -> u64 {
        self.geom.total_sectors
    }

    fn read_block(&mut self, lba: u64) -> io::Result<Vec<u8>> {
        let (cyl, head, sec) = lba_to_chs(&self.geom, lba);
        if self.cur_track != Some((cyl, head)) {
            self.read_track_into_cache(cyl, head, self.revs, true)?;
        }
        // Решение №9: капризный сектор дочитываем бо́льшим числом оборотов,
        // сливая удачные копии, и лишь потом сдаёмся с EIO.
        let mut attempt = 0;
        while !self.have_good(sec) && attempt < 2 {
            attempt += 1;
            let revs = self.revs + attempt as u16 * 3;
            log::info!("сектор C{cyl} H{head} R{sec}: ретрай {revs} оборотов");
            self.read_track_into_cache(cyl, head, revs, false)?;
        }
        match self.cur_sectors.get(&sec) {
            Some(s) if s.data_crc_ok => Ok(s.data.clone()),
            // Решение №9: по умолчанию строгий EIO на битом/пропавшем секторе.
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("сектор C{cyl} H{head} R{sec}: CRC не сошёлся"),
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("сектор C{cyl} H{head} R{sec} не найден на дорожке"),
            )),
        }
    }
}

impl Drop for GreaseweazleSource {
    fn drop(&mut self) {
        // Погасить мотор и отпустить привод.
        let _ = self.gw.set_motor(self.unit, false);
        let _ = self.gw.deselect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lba_chs_1440k() {
        // 1.44МБ: 2 головки, 18 spt.
        let g = Geometry {
            cylinders: 80,
            heads: 2,
            sectors_per_track: 18,
            bytes_per_sector: 512,
            total_sectors: 2880,
        };
        assert_eq!(lba_to_chs(&g, 0), (0, 0, 1)); // boot
        assert_eq!(lba_to_chs(&g, 17), (0, 0, 18)); // конец дорожки 0/0
        assert_eq!(lba_to_chs(&g, 18), (0, 1, 1)); // дорожка 0/1
        assert_eq!(lba_to_chs(&g, 36), (1, 0, 1)); // цилиндр 1
        assert_eq!(lba_to_chs(&g, 2879), (79, 1, 18)); // последний
    }

    #[test]
    fn parse_bpb_from_real_image() {
        // Boot-сектор реального образа MS-DOS 5.0 (fixtures/msdos5.img).
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/msdos5.img");
        let Ok(img) = std::fs::read(path) else {
            eprintln!("fixture отсутствует — пропуск");
            return;
        };
        let g = parse_bpb(&img[..512]).expect("BPB");
        assert_eq!(g.bytes_per_sector, 512);
        assert_eq!(g.sectors_per_track, 18);
        assert_eq!(g.heads, 2);
        assert_eq!(g.total_sectors, 2880);
        assert_eq!(g.cylinders, 80);
    }
}
