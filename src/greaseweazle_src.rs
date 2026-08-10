//! Шаг 3: `BlockSource` поверх живой дискеты через Greaseweazle.
//!
//! Геометрию берём из BPB (FAT) или распознаём TR-DOS (256B сектора). Если
//! сторона 0 не читается, автоматически пробуем сторону 1 и инвертируем
//! head_map (актуально для односторонних TR-DOS дискет).

use std::collections::{BTreeSet, HashMap};
use std::io;

use crate::block_source::BlockSource;
use crate::gw::{BusType, Greaseweazle};
use crate::mfm;

// ---------------------------------------------------------------------------
// Геометрия и формат
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors_per_track: u8,
    pub bytes_per_sector: u16,
    pub total_sectors: u64,
}

/// Тип обнаруженного формата дискеты.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskKind {
    Fat,   // FAT12/16 с BPB на дорожке 0
    TrDos, // TR-DOS (ZX Spectrum): 256-байтовые секторы
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

/// Определить геометрию TR-DOS из набора секторов дорожки 0.
///
/// TR-DOS: 256-байтовые секторы, номера R=1..spt. Определяем spt из
/// максимального встреченного R среди CRC-валидных секторов (или всех).
/// Число цилиндров берём 80 (стандарт), число сторон — 1 (SS).
pub fn trdos_geometry(track0: &[mfm::Sector]) -> Option<Geometry> {
    let sz256: Vec<&mfm::Sector> = track0.iter().filter(|s| s.size_code == 1).collect();
    if sz256.len() < 4 {
        return None; // слишком мало 256-байтовых секторов — вряд ли TR-DOS
    }
    let max_r = sz256.iter()
        .filter(|s| s.data_crc_ok)
        .map(|s| s.sector)
        .max()
        .unwrap_or_else(|| sz256.iter().map(|s| s.sector).max().unwrap_or(16));
    let spt = max_r.max(9).min(16); // разумные пределы
    let cylinders: u16 = 80;
    let heads: u8 = 1; // SS — уточняем при желании по volume descriptor
    let total = cylinders as u64 * heads as u64 * spt as u64;
    log::info!(
        "TR-DOS геометрия: {} cyl × {} hd × {} spt × 256 B = {} секторов",
        cylinders, heads, spt, total
    );
    Some(Geometry {
        cylinders,
        heads,
        sectors_per_track: spt,
        bytes_per_sector: 256,
        total_sectors: total,
    })
}

// ---------------------------------------------------------------------------
// LBA ↔ CHS
// ---------------------------------------------------------------------------

/// LBA → (цилиндр, логическая головка, номер сектора с 1).
pub fn lba_to_chs(g: &Geometry, lba: u64) -> (u16, u8, u8) {
    let spt = g.sectors_per_track as u64;
    let per_cyl = spt * g.heads as u64;
    let cyl = (lba / per_cyl) as u16;
    let rem = lba % per_cyl;
    let head = (rem / spt) as u8;
    let sector = (rem % spt) as u8 + 1;
    (cyl, head, sector)
}

// ---------------------------------------------------------------------------
// Источник блоков
// ---------------------------------------------------------------------------

pub struct GreaseweazleSource {
    gw: Greaseweazle,
    pub geom: Geometry,
    pub disk_kind: DiskKind,
    unit: u8,
    revs: u16,
    recover: bool,
    /// Маппинг логической головки → физический head GW.
    /// [0,1] = прямой; [1,0] = инвертированный (сторона 0 читается head 1).
    head_map: [u8; 2],
    cur_track: Option<(u16, u8)>,
    cur_sectors: HashMap<u8, mfm::Sector>,
    bad: BTreeSet<(u16, u8, u8)>,
}

impl GreaseweazleSource {
    /// Открыть привод, запустить мотор, определить формат по дорожке 0.
    ///
    /// Алгоритм:
    ///  1. Читаем дорожку 0 головки 0.
    ///  2. Если нашли BPB → FAT, head_map=[0,1].
    ///  3. Если нашли 256B-секторов ≥4 → TR-DOS, head_map=[0,1].
    ///  4. Иначе читаем головку 1 и повторяем → если удача, head_map=[1,0].
    pub fn open(
        port: &str,
        unit: u8,
        bus: BusType,
        hd: bool,
        revs: u16,
        recover: bool,
    ) -> io::Result<Self> {
        log::info!("подключаюсь к Greaseweazle на {port}…");
        let mut gw = Greaseweazle::open(port)?;
        gw.set_bus_type(bus)?;
        gw.select(unit)?;
        gw.set_motor(unit, true)?;
        if hd {
            log::info!("HD режим: asserting density-select (pin 2 low)");
            if let Err(e) = gw.set_pin(2, false) {
                log::warn!(
                    "set_pin(2, false) вернул ошибку: {e}\n  \
                     Это может означать, что прошивка не поддерживает SET_PIN (opcode 19) \
                     для текущей конфигурации, или нужен другой номер пина.\n  \
                     Проверь: vf activate greaseweazle && python -c \
                     \"import greaseweazle.usb as u; print(u.Cmd.SetPin)\""
                );
            }
        }
        log::info!(
            "привод {unit} выбран, мотор запущен{}; раскрутка 800 мс…",
            if hd { " [HD]" } else { "" }
        );
        std::thread::sleep(std::time::Duration::from_millis(800));

        // Читаем дорожку 0 с головки 0.
        gw.seek(0)?;
        log::info!("читаю дорожку 0 головку 0 (определение формата)…");
        gw.select_head(0)?;
        let flux0 = gw.read_flux(revs)?;
        let secs0 = mfm::decode_track(&flux0);
        log_track_summary(0, 0, &secs0);

        // Пытаемся определить формат по головке 0.
        if let Some((geom, disk_kind)) = detect_format(&secs0) {
            log::info!("формат определён по head 0: {disk_kind:?}");
            return Ok(GreaseweazleSource::new(gw, geom, disk_kind, unit, revs, recover, [0, 1]));
        }

        // Головка 0 пустая или нечитаемая — пробуем головку 1.
        log::info!("head 0 не содержит распознанного формата, пробую head 1…");
        gw.select_head(1)?;
        let flux1 = gw.read_flux(revs)?;
        let secs1 = mfm::decode_track(&flux1);
        log_track_summary(0, 1, &secs1);

        if let Some((geom, disk_kind)) = detect_format(&secs1) {
            log::info!("формат определён по head 1: {disk_kind:?}, head_map=[1,0]");
            return Ok(GreaseweazleSource::new(gw, geom, disk_kind, unit, revs, recover, [1, 0]));
        }

        // Формат не определён ни по одной из сторон.
        let ids0: Vec<u8> = secs0.iter().map(|s| s.sector).collect();
        let ids1: Vec<u8> = secs1.iter().map(|s| s.sector).collect();
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "формат дискеты не распознан.\n  \
                 Head 0: {} секторов, R={ids0:?}\n  \
                 Head 1: {} секторов, R={ids1:?}\n  \
                 Попробуй: -v (подробный лог), --hd (5.25\" HD привод), \
                 --revs 5 (ненадёжные дискеты).",
                secs0.len(),
                secs1.len()
            ),
        ))
    }

    fn new(
        gw: Greaseweazle,
        geom: Geometry,
        disk_kind: DiskKind,
        unit: u8,
        revs: u16,
        recover: bool,
        head_map: [u8; 2],
    ) -> Self {
        GreaseweazleSource {
            gw,
            geom,
            disk_kind,
            unit,
            revs,
            recover,
            head_map,
            cur_track: None,
            cur_sectors: HashMap::new(),
            bad: BTreeSet::new(),
        }
    }

    /// Физически прочитать дорожку. Логическая головка `head` транслируется в
    /// физическую через `head_map` (инвертирование для TR-DOS на головке 1).
    fn read_track_into_cache(
        &mut self,
        cyl: u16,
        head: u8,
        revs: u16,
        fresh: bool,
    ) -> io::Result<()> {
        let phys_head = self.head_map[head as usize];
        log::info!("чтение дорожки C{cyl} L{head}→P{phys_head} ({revs} об.)…");
        self.gw.select(self.unit)?;
        self.gw.set_motor(self.unit, true)?;
        self.gw.seek(cyl as u8)?;
        self.gw.select_head(phys_head)?;
        let flux = self.gw.read_flux(revs)?;
        let sectors = mfm::decode_track(&flux);
        let good = sectors.iter().filter(|s| s.data_crc_ok).count();
        log::info!("C{cyl} H{head}: {} секторов, валидных {good}", sectors.len());
        if good < sectors.len() {
            for s in sectors.iter().filter(|s| !s.data_crc_ok) {
                log::debug!("  битый: C{} H{} R{} {}B id_crc={}", s.cyl, s.head, s.sector, s.size(), s.id_crc_ok);
            }
        }

        if fresh {
            self.cur_sectors.clear();
            self.cur_track = Some((cyl, head));
        }
        for s in sectors {
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

    fn load(&mut self, cyl: u16, head: u8, revs: u16, fresh: bool) -> io::Result<()> {
        match self.read_track_into_cache(cyl, head, revs, fresh) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotConnected => Err(e),
            Err(e) if self.recover => {
                if fresh {
                    self.cur_sectors.clear();
                    self.cur_track = Some((cyl, head));
                }
                log::warn!("recover: дорожка C{cyl} H{head} не прочиталась ({e})");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Вспомогательные функции определения формата
// ---------------------------------------------------------------------------

fn detect_format(sectors: &[mfm::Sector]) -> Option<(Geometry, DiskKind)> {
    // Сначала пробуем FAT/BPB: ищем сектор R=1 с 512B данными и валидным CRC.
    if let Some(boot) = sectors
        .iter()
        .find(|s| s.sector == 1 && s.size_code == 2 && s.data_crc_ok)
        .or_else(|| sectors.iter().find(|s| s.sector == 1 && s.size_code == 2))
    {
        if let Some(geom) = parse_bpb(&boot.data) {
            return Some((geom, DiskKind::Fat));
        }
        // R=1 есть, но BPB не распознан — логируем диагностику.
        let hex: String = boot.data[..boot.data.len().min(32)]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        log::info!("R=1 найден, но BPB не распознан. Первые 32B: [{hex}]");
    }

    // Пробуем TR-DOS: ищем 256B-секторов.
    if let Some(geom) = trdos_geometry(sectors) {
        return Some((geom, DiskKind::TrDos));
    }

    None
}

fn log_track_summary(cyl: u8, head: u8, sectors: &[mfm::Sector]) {
    let good = sectors.iter().filter(|s| s.data_crc_ok).count();
    let mut ids: Vec<u8> = sectors.iter().map(|s| s.sector).collect();
    ids.sort_unstable();
    ids.dedup();
    log::info!("дорожка C{cyl} H{head}: {} секторов ({good} CRC-ok), R={ids:?}", sectors.len());
}

// ---------------------------------------------------------------------------
// BlockSource impl
// ---------------------------------------------------------------------------

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
            self.load(cyl, head, self.revs, true)?;
        }
        let mut attempt = 0;
        while !self.have_good(sec) && attempt < 2 {
            attempt += 1;
            let revs = self.revs + attempt as u16 * 3;
            log::info!("сектор C{cyl} H{head} R{sec}: ретрай {revs} оборотов");
            self.load(cyl, head, revs, false)?;
        }
        if let Some(data) = self
            .cur_sectors
            .get(&sec)
            .filter(|s| s.data_crc_ok)
            .map(|s| s.data.clone())
        {
            return Ok(data);
        }

        if self.recover {
            self.bad.insert((cyl, head, sec));
            if let Some(data) = self.cur_sectors.get(&sec).map(|s| s.data.clone()) {
                log::warn!("recover: C{cyl} H{head} R{sec} — отдаю данные с плохим CRC");
                return Ok(data);
            }
            log::warn!("recover: C{cyl} H{head} R{sec} отсутствует — заполняю нулями");
            return Ok(vec![0u8; self.geom.bytes_per_sector as usize]);
        }

        self.default_read_failure(cyl, head, sec)
    }
}

impl GreaseweazleSource {
    fn default_read_failure(&self, cyl: u16, head: u8, sec: u8) -> io::Result<Vec<u8>> {
        match self.cur_sectors.get(&sec) {
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("сектор C{cyl} H{head} R{sec}: CRC не сошёлся (пробуй --recover)"),
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("сектор C{cyl} H{head} R{sec} не найден (пробуй --recover)"),
            )),
        }
    }
}

impl Drop for GreaseweazleSource {
    fn drop(&mut self) {
        if !self.bad.is_empty() {
            eprintln!("Проблемных секторов: {} (C/H/R):", self.bad.len());
            for (c, h, r) in &self.bad {
                eprintln!("  C{c} H{h} R{r}");
            }
        }
        let _ = self.gw.set_motor(self.unit, false);
        let _ = self.gw.deselect();
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lba_chs_1440k() {
        let g = Geometry {
            cylinders: 80,
            heads: 2,
            sectors_per_track: 18,
            bytes_per_sector: 512,
            total_sectors: 2880,
        };
        assert_eq!(lba_to_chs(&g, 0), (0, 0, 1));
        assert_eq!(lba_to_chs(&g, 17), (0, 0, 18));
        assert_eq!(lba_to_chs(&g, 18), (0, 1, 1));
        assert_eq!(lba_to_chs(&g, 36), (1, 0, 1));
        assert_eq!(lba_to_chs(&g, 2879), (79, 1, 18));
    }

    #[test]
    fn lba_chs_trdos_ss14() {
        // TR-DOS SS 80 треков 14 сpt
        let g = Geometry {
            cylinders: 80,
            heads: 1,
            sectors_per_track: 14,
            bytes_per_sector: 256,
            total_sectors: 1120,
        };
        assert_eq!(lba_to_chs(&g, 0), (0, 0, 1));   // track0 sec1
        assert_eq!(lba_to_chs(&g, 13), (0, 0, 14));  // track0 sec14
        assert_eq!(lba_to_chs(&g, 14), (1, 0, 1));   // track1 sec1
    }

    #[test]
    fn parse_bpb_from_real_image() {
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
