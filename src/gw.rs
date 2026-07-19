//! Свой хост-протокол Greaseweazle поверх USB-serial (CDC).
//!
//! Реализовано из ОФИЦИАЛЬНОГО public-domain `usb.py`/прошивки (Unlicense),
//! не из стороннего EUPL-крейта — поэтому лицензия у нас свободная. API узкий:
//! только то, что нужно для read-only монтирования (без write/erase/bandwidth).
//!
//! Фрейминг: пишем `[opcode, len, params…]`, читаем 2 байта `[echo_opcode,
//! result]`; result 0 = Ok. Поток флукса — переменной длины, декодируется в
//! интервалы (тики) в [`decode_flux`].
//!
//! Serial-часть проверяется только на живом Визеле; кодек флукса — оффлайн
//! (round-trip тест ниже).

use std::io::{self, Read, Write};
use std::time::Duration;

use serialport::SerialPort;

// --- опкоды команд (usb.py Cmd) ---
mod cmd {
    pub const GET_INFO: u8 = 0;
    pub const SEEK: u8 = 2;
    pub const HEAD: u8 = 3;
    pub const MOTOR: u8 = 6;
    pub const READ_FLUX: u8 = 7;
    pub const GET_FLUX_STATUS: u8 = 9;
    pub const SELECT: u8 = 12;
    pub const DESELECT: u8 = 13;
    pub const SET_BUS_TYPE: u8 = 14;
    pub const GET_PIN: u8 = 20;
}

// --- опкоды внутри флукс-потока (usb.py FluxOp) ---
mod fluxop {
    pub const INDEX: u8 = 1;
    pub const SPACE: u8 = 2;
}

/// Тип шины (usb.py BusType).
#[derive(Clone, Copy)]
pub enum BusType {
    Ibmpc = 1,
    Shugart = 2,
}

/// Пины, читаемые с привода (подмножество usb.py Pin).
#[derive(Clone, Copy)]
pub enum Pin {
    Index = 8,
    Track0 = 26,
    WriteProtect = 28,
    DiskChange = 34, // Low = диск менялся с прошлого доступа (IBM PC) — для авто-детекта
}

#[derive(Debug)]
pub struct FirmwareInfo {
    pub major: u8,
    pub minor: u8,
    pub sample_freq: u32, // Гц; тики read_flux считаются на этой частоте
}

/// Список последовательных портов, среди которых искать Визель.
pub fn enumerate() -> io::Result<Vec<String>> {
    let ports = serialport::available_ports()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    Ok(ports.into_iter().map(|p| p.port_name).collect())
}

pub struct Greaseweazle {
    port: Box<dyn SerialPort>,
    path: String,
    sample_freq: u32,
}

fn open_port(path: &str) -> io::Result<Box<dyn SerialPort>> {
    // GW — CDC-устройство; скорость номинальная, важен щедрый таймаут:
    // чтение флукса дорожки длится сотни мс.
    serialport::new(path, 115_200)
        .timeout(Duration::from_secs(15))
        .open()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
}

impl Greaseweazle {
    /// Открыть Визель на указанном порту (например /dev/tty.usbmodemGW…).
    pub fn open(path: &str) -> io::Result<Self> {
        let mut gw = Greaseweazle {
            port: open_port(path)?,
            path: path.to_string(),
            sample_freq: 0,
        };
        gw.reset()?;
        let info = gw.get_info()?;
        gw.sample_freq = info.sample_freq;
        Ok(gw)
    }

    /// «Clear comms» ресинхронизация (usb.py reset): сброс буферов + переключение
    /// baudrate на магическое ClearComms и обратно. Приводит прошивку в известное
    /// состояние и выравнивает поток команд.
    ///
    /// ВАЖНО: не пере-открываем порт здесь — `serialport` держит его эксклюзивно
    /// (TIOCEXCL), и второй `open` того же узла упал бы с EBUSY на самого себя.
    /// Затем — короткий дренаж остаточных байт (ограничен по времени).
    fn reset(&mut self) -> io::Result<()> {
        const CLEAR_COMMS: u32 = 10000;
        const NORMAL: u32 = 9600;
        let _ = self.port.clear(serialport::ClearBuffer::Output);
        let _ = self.port.set_baud_rate(CLEAR_COMMS);
        let _ = self.port.set_baud_rate(NORMAL);
        let _ = self.port.clear(serialport::ClearBuffer::Input);

        // Дренаж остаточных байт (не дольше ~1с), чтобы ack не разъехался.
        let _ = self.port.set_timeout(Duration::from_millis(150));
        let mut junk = [0u8; 4096];
        let start = std::time::Instant::now();
        while let Ok(n) = self.port.read(&mut junk) {
            if n == 0 || start.elapsed() > Duration::from_millis(1000) {
                break;
            }
        }
        let _ = self.port.set_timeout(Duration::from_secs(15));
        Ok(())
    }

    pub fn sample_freq(&self) -> u32 {
        self.sample_freq
    }

    // --- нижний уровень: отправка команды и проверка ответа ---

    fn send_cmd(&mut self, cmd: &[u8]) -> io::Result<()> {
        self.port.write_all(cmd)?;
        let mut ack = [0u8; 2];
        self.port.read_exact(&mut ack)?;
        if ack[0] != cmd[0] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("эхо команды {} != {}", ack[0], cmd[0]),
            ));
        }
        if ack[1] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("команда {} → ошибка {}", cmd[0], ack[1]),
            ));
        }
        Ok(())
    }

    pub fn get_info(&mut self) -> io::Result<FirmwareInfo> {
        // [GetInfo, len=3, index=Firmware(0)] затем 32 байта payload.
        self.send_cmd(&[cmd::GET_INFO, 3, 0])?;
        let mut buf = [0u8; 32];
        self.port.read_exact(&mut buf)?;
        // Раскладка: major, minor, is_main, max_cmd, sample_freq(u32 LE), …
        let sample_freq = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        Ok(FirmwareInfo {
            major: buf[0],
            minor: buf[1],
            sample_freq,
        })
    }

    pub fn set_bus_type(&mut self, bus: BusType) -> io::Result<()> {
        self.send_cmd(&[cmd::SET_BUS_TYPE, 3, bus as u8])
    }

    pub fn select(&mut self, unit: u8) -> io::Result<()> {
        self.send_cmd(&[cmd::SELECT, 3, unit])
    }

    pub fn deselect(&mut self) -> io::Result<()> {
        self.send_cmd(&[cmd::DESELECT, 2])
    }

    pub fn set_motor(&mut self, unit: u8, on: bool) -> io::Result<()> {
        self.send_cmd(&[cmd::MOTOR, 4, unit, on as u8])
    }

    pub fn seek(&mut self, cylinder: u8) -> io::Result<()> {
        // Дискетные цилиндры 0..83 помещаются в байт (знаковый в прошивке).
        self.send_cmd(&[cmd::SEEK, 3, cylinder])
    }

    pub fn select_head(&mut self, head: u8) -> io::Result<()> {
        self.send_cmd(&[cmd::HEAD, 3, head])
    }

    pub fn get_pin(&mut self, pin: Pin) -> io::Result<bool> {
        // Возвращает уровень пина: true = High, false = Low.
        self.send_cmd(&[cmd::GET_PIN, 3, pin as u8])?;
        let mut b = [0u8; 1];
        self.port.read_exact(&mut b)?;
        Ok(b[0] != 0)
    }

    /// Прочитать флукс текущей дорожки за `revs` оборотов. Мотор должен крутиться.
    /// Возвращает интервалы перемагничиваний в тиках (индекс-импульсы отброшены).
    pub fn read_flux(&mut self, revs: u16) -> io::Result<Vec<u32>> {
        // [ReadFlux, len=8, ticks(u32)=0(без лимита по времени), nr_index(u16)]
        let nr = if revs == 0 { 0 } else { revs + 1 };
        let mut c = vec![cmd::READ_FLUX, 8];
        c.extend_from_slice(&0u32.to_le_bytes());
        c.extend_from_slice(&nr.to_le_bytes());
        self.send_cmd(&c)?;

        let raw = self.read_until_zero()?;
        // Завершаем чтение и проверяем статус потока.
        self.send_cmd(&[cmd::GET_FLUX_STATUS, 2])?;

        let (flux, _index) = decode_flux(&raw);
        Ok(flux)
    }

    /// Прочитать байты из порта до терминатора 0 включительно (0 в потоке
    /// флукса встречается только как маркер конца).
    fn read_until_zero(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(64 * 1024);
        let mut chunk = [0u8; 4096];
        loop {
            let n = self.port.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "поток флукса оборвался без терминатора",
                ));
            }
            out.extend_from_slice(&chunk[..n]);
            if out.last() == Some(&0) {
                break;
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Декодирование проводного формата флукс-потока (из usb.py _decode_flux)
// ---------------------------------------------------------------------------

/// Декодировать сырой поток в (интервалы флукса в тиках, времена индекс-импульсов).
pub fn decode_flux(dat: &[u8]) -> (Vec<u32>, Vec<u32>) {
    let mut flux = Vec::new();
    let mut index = Vec::new();
    let mut ticks: u32 = 0;
    let mut ticks_since_index: u32 = 0;

    let mut it = dat.iter().copied().peekable();
    // 28-битное значение: по 7 бит в каждом байте (бит0 отброшен маской 254).
    fn read28(it: &mut impl Iterator<Item = u8>) -> u32 {
        let b0 = (it.next().unwrap_or(0) & 254) as u32 >> 1;
        let b1 = (it.next().unwrap_or(0) & 254) as u32 >> 1;
        let b2 = (it.next().unwrap_or(0) & 254) as u32 >> 1;
        let b3 = (it.next().unwrap_or(0) & 254) as u32 >> 1;
        b0 | (b1 << 7) | (b2 << 14) | (b3 << 21)
    }

    while let Some(i) = it.next() {
        if i == 0 {
            break; // терминатор
        } else if i == 255 {
            match it.next() {
                Some(fluxop::INDEX) => {
                    let val = read28(&mut it);
                    index.push(ticks_since_index + val);
                    ticks_since_index = 0;
                }
                Some(fluxop::SPACE) => {
                    ticks += read28(&mut it);
                }
                _ => {} // неизвестный опкод — пропускаем
            }
        } else if i < 250 {
            ticks += i as u32;
            flux.push(ticks);
            ticks_since_index += ticks;
            ticks = 0;
        } else {
            // среднее значение: 250 + (i-250)*255 + (next-1).
            // saturating_sub: на мусорном потоке next может быть 0 (иначе underflow-паника).
            let next = it.next().unwrap_or(1) as u32;
            let val = 250 + (i as u32 - 250) * 255 + next.saturating_sub(1);
            ticks += val;
            flux.push(ticks);
            ticks_since_index += ticks;
            ticks = 0;
        }
    }
    (flux, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Кодировщик проводного формата (только для round-trip теста).
    fn encode_flux(intervals: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for &v in intervals {
            if v < 250 {
                out.push(v as u8);
            } else if v < 250 + 5 * 255 {
                let q = v - 250;
                out.push((250 + q / 255) as u8);
                out.push((q % 255 + 1) as u8);
            } else {
                // Space-опкод + 28-битное значение, затем нулевой флукс не нужен;
                // для простоты кодируем как один большой интервал через Space+малый.
                out.push(255);
                out.push(fluxop::SPACE);
                let val = v - 1;
                out.push((((val) & 0x7f) << 1) as u8 | 1);
                out.push((((val >> 7) & 0x7f) << 1) as u8 | 1);
                out.push((((val >> 14) & 0x7f) << 1) as u8 | 1);
                out.push((((val >> 21) & 0x7f) << 1) as u8 | 1);
                out.push(1); // завершающий малый флукс (+1 тик), поглотит space
            }
        }
        out.push(0); // терминатор
        out
    }

    #[test]
    fn flux_wire_roundtrip_small_and_medium() {
        // Типичные интервалы дискеты (тики при ~72МГц): 100..900.
        let intervals: Vec<u32> = (100..900).step_by(37).collect();
        let raw = encode_flux(&intervals);
        let (flux, _idx) = decode_flux(&raw);
        assert_eq!(flux, intervals);
    }

    #[test]
    fn flux_wire_decodes_literals() {
        // 3,7,200 (все <250) + терминатор.
        let raw = vec![3u8, 7, 200, 0];
        let (flux, _idx) = decode_flux(&raw);
        assert_eq!(flux, vec![3, 7, 200]);
    }

    #[test]
    fn flux_wire_index_pulse() {
        // малый флукс 100, затем Index-опкод со значением 5, затем флукс 50.
        let mut raw = vec![100u8, 255, fluxop::INDEX];
        for shift in [0u32, 7, 14, 21] {
            raw.push((((5u32 >> shift) & 0x7f) << 1) as u8 | 1);
        }
        raw.push(50);
        raw.push(0);
        let (flux, index) = decode_flux(&raw);
        assert_eq!(flux, vec![100, 50]);
        assert_eq!(index, vec![105]); // ticks_since_index(100) + val(5)
    }
}
