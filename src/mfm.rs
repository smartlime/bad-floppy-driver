//! Декодер IBM System 34 / PC MFM: поток флукса → секторы.
//!
//! Крейт `greaseweazle` (и наш будущий `gw.rs`) отдают только флукс — интервалы
//! между перемагничиваниями. Всё, что выше, пишем сами. Это тот самый
//! «низкоуровневый» кусок, и он тестируется оффлайн: здесь же есть MFM-энкодер
//! (секторы → флукс), поэтому round-trip проверяется без железа.
//!
//! Конвейер декода:
//!   флукс-интервалы → (PLL) битовые ячейки (cells) → поиск sync 0x4489
//!   → байты → IDAM (C/H/R/N + CRC) и DAM (данные + CRC).
//!
//! Термины: одна «ячейка» (cell) — это полбита MFM (чередуются clock и data).
//! Перемагничивание = ячейка со значением 1. Легальные интервалы MFM — 2, 3
//! или 4 ячейки.

/// CRC16-CCITT (poly 0x1021, init 0xFFFF, без реверса) — как на дискетах.
pub fn crc16(seed: u16, data: &[u8]) -> u16 {
    let mut crc = seed;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^  0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Ячейки sync-метки A1 (0x4489) — A1 с намеренно пропущенным клоком.
const SYNC_A1: u16 = 0x4489;

const IDAM: u8 = 0xFE; // ID Address Mark
const DAM: u8 = 0xFB; // Data Address Mark
const DAM_DELETED: u8 = 0xF8; // Deleted Data Address Mark

/// Один декодированный сектор дорожки.
#[derive(Debug, Clone)]
pub struct Sector {
    pub cyl: u8,
    pub head: u8,
    pub sector: u8, // R — номер сектора (обычно с 1)
    pub size_code: u8, // N: размер = 128 << N (обычно 2 => 512)
    pub data: Vec<u8>,
    pub id_crc_ok: bool,
    pub data_crc_ok: bool,
    pub deleted: bool,
}

impl Sector {
    pub fn size(&self) -> usize {
        128usize << self.size_code
    }
}

// ---------------------------------------------------------------------------
// Декодирование
// ---------------------------------------------------------------------------

/// Декодировать все секторы дорожки из флукс-интервалов (в тиках `read_flux`).
///
/// Период ячейки оценивается автоматически из самих данных и подстраивается
/// программным PLL, поэтому DD/HD и небольшой разброс скорости обрабатываются
/// без внешних подсказок.
pub fn decode_track(flux_ticks: &[u32]) -> Vec<Sector> {
    if flux_ticks.len() < 16 {
        return Vec::new();
    }
    let cells = flux_to_cells(flux_ticks);
    decode_cells(&cells)
}

/// Флукс-интервалы → битовые ячейки (true = перемагничивание) через PLL.
fn flux_to_cells(flux_ticks: &[u32]) -> Vec<bool> {
    // Начальная оценка периода ячейки: ~5-й перцентиль интервалов ≈ «2 ячейки».
    let mut sorted: Vec<u32> = flux_ticks.iter().copied().filter(|&t| t > 0).collect();
    sorted.sort_unstable();
    let p5 = sorted[sorted.len() / 20];
    let mut cell = (p5 as f64 / 2.0).max(1.0);

    let mut cells = Vec::with_capacity(flux_ticks.len() * 3);
    for &t in flux_ticks {
        if t == 0 {
            continue;
        }
        // Число ячеек в интервале. НЕ клампим до легальных 2..4: доверяем таймингу,
        // иначе один шумный (длинный) интервал сдвинул бы выравнивание всей дорожки.
        let n = (t as f64 / cell).round().max(1.0) as i64;
        // Подстройка PLL — мягкая и по «сырому» соотношению t/n, чтобы нелегальный
        // интервал не уводил оценку периода (иначе ошибки идут каскадом).
        cell = cell * 0.9 + (t as f64 / n as f64) * 0.1;
        for _ in 0..(n - 1) {
            cells.push(false);
        }
        cells.push(true);
    }
    cells
}

/// Прочитать 16-битное окно ячеек начиная с позиции `p` (MSB первым).
fn window16(cells: &[bool], p: usize) -> Option<u16> {
    if p + 16 > cells.len() {
        return None;
    }
    let mut v = 0u16;
    for i in 0..16 {
        v = (v << 1) | (cells[p + i] as u16);
    }
    Some(v)
}

/// Декодировать один MFM-байт: data-биты — нечётные ячейки (p+1, p+3, …, p+15).
fn decode_byte(cells: &[bool], p: usize) -> Option<u8> {
    if p + 16 > cells.len() {
        return None;
    }
    let mut b = 0u8;
    for k in 0..8 {
        b = (b << 1) | (cells[p + 2 * k + 1] as u8);
    }
    Some(b)
}

/// Прочитать `count` последовательных байт начиная с позиции ячейки `p`.
fn read_bytes(cells: &[bool], p: usize, count: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(decode_byte(cells, p + i * 16)?);
    }
    Some(out)
}

fn decode_cells(cells: &[bool]) -> Vec<Sector> {
    // Позиции всех sync-меток 0x4489.
    let mut syncs = Vec::new();
    let mut i = 0;
    while i + 16 <= cells.len() {
        if window16(cells, i) == Some(SYNC_A1) {
            syncs.push(i);
            i += 16; // следующая метка не может начаться раньше
        } else {
            i += 1;
        }
    }

    // Сгруппировать подряд идущие метки (обычно по 3 перед адресной меткой).
    let mut runs: Vec<(usize, usize)> = Vec::new(); // (start_pos, count)
    for &s in &syncs {
        match runs.last_mut() {
            Some((start, cnt)) if s == *start + *cnt * 16 => *cnt += 1,
            _ => runs.push((s, 1)),
        }
    }

    let mut sectors: Vec<Sector> = Vec::new();
    let mut pending_id: Option<(u8, u8, u8, u8, bool)> = None; // C,H,R,N,id_crc_ok

    for (start, cnt) in runs {
        let a1n = cnt.min(3); // для CRC учитываем ровно синхронизацию A1×3
        let mark_pos = start + cnt * 16;
        let Some(mark) = decode_byte(cells, mark_pos) else {
            continue;
        };
        let a1s = vec![0xA1u8; a1n];

        match mark {
            IDAM => {
                let Some(body) = read_bytes(cells, mark_pos + 16, 6) else {
                    continue;
                };
                let (c, h, r, n) = (body[0], body[1], body[2], body[3]);
                let stored = u16::from_be_bytes([body[4], body[5]]);
                let mut crc_in = a1s.clone();
                crc_in.push(mark);
                crc_in.extend_from_slice(&body[..4]);
                let ok = crc16(0xFFFF, &crc_in) == stored;
                pending_id = Some((c, h, r, n, ok));
            }
            DAM | DAM_DELETED => {
                let (c, h, r, n, id_ok) = pending_id.take().unwrap_or((0, 0, 0, 2, false));
                let size = 128usize << n;
                let Some(payload) = read_bytes(cells, mark_pos + 16, size + 2) else {
                    continue;
                };
                let data = payload[..size].to_vec();
                let stored = u16::from_be_bytes([payload[size], payload[size + 1]]);
                let mut crc_in = a1s.clone();
                crc_in.push(mark);
                crc_in.extend_from_slice(&data);
                let data_ok = crc16(0xFFFF, &crc_in) == stored;
                sectors.push(Sector {
                    cyl: c,
                    head: h,
                    sector: r,
                    size_code: n,
                    data,
                    id_crc_ok: id_ok,
                    data_crc_ok: data_ok,
                    deleted: mark == DAM_DELETED,
                });
            }
            _ => {}
        }
    }
    sectors
}

// ---------------------------------------------------------------------------
// Кодирование (для round-trip тестов; на реальном железе не нужно — read-only)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod encode {
    use super::*;

    /// Добавить 16 ячеек sync-метки A1 (0x4489).
    fn push_sync(cells: &mut Vec<bool>) {
        for i in (0..16).rev() {
            cells.push((SYNC_A1 >> i) & 1 == 1);
        }
    }

    /// Закодировать обычный байт в 16 MFM-ячеек. `prev` — последний data-бит.
    fn push_byte(cells: &mut Vec<bool>, byte: u8, prev: &mut bool) {
        for k in (0..8).rev() {
            let d = (byte >> k) & 1 == 1;
            let clock = !*prev && !d; // клок только между двумя нулями
            cells.push(clock);
            cells.push(d);
            *prev = d;
        }
    }

    fn push_bytes(cells: &mut Vec<bool>, bytes: &[u8], prev: &mut bool) {
        for &b in bytes {
            push_byte(cells, b, prev);
        }
    }

    /// Собрать поле адресной метки: 3×A1 + mark + payload + CRC16.
    fn push_am(cells: &mut Vec<bool>, mark: u8, payload: &[u8], prev: &mut bool) {
        push_sync(cells);
        push_sync(cells);
        push_sync(cells);
        *prev = true; // последний data-бит A1 (0xA1) = 1
        let mut crc_in = vec![0xA1u8, 0xA1, 0xA1, mark];
        crc_in.extend_from_slice(payload);
        let crc = crc16(0xFFFF, &crc_in);
        push_byte(cells, mark, prev);
        push_bytes(cells, payload, prev);
        push_bytes(cells, &crc.to_be_bytes(), prev);
    }

    /// Закодировать один сектор (IDAM+GAP+DAM) в ячейки.
    pub fn sector(cyl: u8, head: u8, sec: u8, n: u8, data: &[u8]) -> Vec<bool> {
        let mut cells = Vec::new();
        let mut prev = false;
        // Немного GAP/sync-нулей перед меткой.
        push_bytes(&mut cells, &[0x4E; 12], &mut prev);
        push_bytes(&mut cells, &[0x00; 12], &mut prev);
        push_am(&mut cells, IDAM, &[cyl, head, sec, n], &mut prev);
        push_bytes(&mut cells, &[0x4E; 22], &mut prev);
        push_bytes(&mut cells, &[0x00; 12], &mut prev);
        push_am(&mut cells, DAM, data, &mut prev);
        push_bytes(&mut cells, &[0x4E; 24], &mut prev);
        cells
    }

    /// Ячейки → флукс-интервалы с заданным периодом ячейки (плюс опц. джиттер).
    pub fn cells_to_flux(cells: &[bool], cell_ticks: u32, jitter: &dyn Fn(usize) -> i32) -> Vec<u32> {
        let mut flux = Vec::new();
        let mut last: Option<usize> = None;
        for (i, &c) in cells.iter().enumerate() {
            if c {
                if let Some(l) = last {
                    let base = ((i - l) as i32) * cell_ticks as i32;
                    flux.push((base + jitter(i)).max(1) as u32);
                }
                last = Some(i);
            }
        }
        flux
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_known_vector() {
        // CRC16-CCITT (init 0xFFFF) от "123456789" == 0x29B1.
        assert_eq!(crc16(0xFFFF, b"123456789"), 0x29B1);
    }

    fn nojitter(_: usize) -> i32 {
        0
    }

    #[test]
    fn roundtrip_single_sector() {
        let data: Vec<u8> = (0..512).map(|i| (i * 7 + 3) as u8).collect();
        let cells = encode::sector(10, 1, 5, 2, &data);
        let flux = encode::cells_to_flux(&cells, 1000, &nojitter);

        let secs = decode_track(&flux);
        assert_eq!(secs.len(), 1, "должен декодироваться ровно один сектор");
        let s = &secs[0];
        assert_eq!((s.cyl, s.head, s.sector, s.size_code), (10, 1, 5, 2));
        assert!(s.id_crc_ok, "ID CRC");
        assert!(s.data_crc_ok, "DATA CRC");
        assert_eq!(s.data, data);
    }

    #[test]
    fn roundtrip_survives_speed_jitter() {
        // ±8% псевдослучайный джиттер имитирует разброс скорости привода.
        let data: Vec<u8> = (0..512).map(|i| (i ^ 0x5A) as u8).collect();
        let cells = encode::sector(0, 0, 1, 2, &data);
        let jit = |i: usize| -> i32 {
            let r = ((i as u32).wrapping_mul(2654435761) >> 24) as i32 % 160; // 0..159
            (r - 80) // ±80 тиков при периоде 1000 => ±8%
        };
        let flux = encode::cells_to_flux(&cells, 1000, &jit);
        let secs = decode_track(&flux);
        assert_eq!(secs.len(), 1);
        assert!(secs[0].data_crc_ok);
        assert_eq!(secs[0].data, data);
    }

    #[test]
    fn detects_corrupt_data_crc() {
        let data = vec![0xAAu8; 512];
        let mut cells = encode::sector(1, 0, 2, 2, &data);
        // Data-поле начинается на 72-й «байт-единице» (leading 24 + IDAM 10 +
        // gap 34 + DAM-заголовок 4). Перевернём data-бит байта №200 данных —
        // глубоко внутри, вдали от sync. Выравнивание цело, данные меняются.
        let data_bit = (72 + 200) * 16 + 1;
        cells[data_bit] = !cells[data_bit];
        let flux = encode::cells_to_flux(&cells, 1000, &nojitter);
        let secs = decode_track(&flux);
        assert_eq!(secs.len(), 1, "сектор всё ещё находится по sync");
        assert!(!secs[0].data_crc_ok, "повреждение данных поймано по CRC");
    }

    #[test]
    fn decodes_full_track_of_18_sectors() {
        // Склеим 18 секторов подряд, как на дорожке 1.44МБ.
        let mut cells = Vec::new();
        let mut expected = Vec::new();
        for r in 1..=18u8 {
            let data: Vec<u8> = (0..512).map(|i| (i as u8).wrapping_add(r)).collect();
            cells.extend(encode::sector(0, 0, r, 2, &data));
            expected.push(data);
        }
        let flux = encode::cells_to_flux(&cells, 1000, &nojitter);
        let secs = decode_track(&flux);
        assert_eq!(secs.len(), 18);
        for (idx, s) in secs.iter().enumerate() {
            assert_eq!(s.sector, (idx + 1) as u8);
            assert!(s.data_crc_ok && s.id_crc_ok);
            assert_eq!(s.data, expected[idx]);
        }
    }
}

