//! Поток-актор, владеющий устройством и дорожечным кэшем (решения №6 и №10).
//!
//! FUSE-колбэки (через `fatfs`) работают в потоке `mount2`, а медленное
//! устройство живёт здесь, в отдельном потоке. Общение — через канал:
//! колбэк шлёт `Request::ReadBlock` и блокируется на ответе. Всё состояние
//! (кэш) принадлежит одному потоку, поэтому никаких `Mutex`/`RwLock`.
//!
//! Кэш — дорожечный: физическая единица чтения дискеты — дорожка, а не сектор.
//! Промах по любому сектору дорожки читает всю дорожку целиком и кладёт в кэш;
//! дальше все её секторы отдаются мгновенно. Для `ImageFile` «чтение дорожки» —
//! это просто `spt` последовательных секторов; для Greaseweazle (шаг 3) — один
//! проход флукса по дорожке.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::block_source::BlockSource;

enum Request {
    ReadBlock {
        lba: u64,
        resp: Sender<io::Result<Vec<u8>>>,
    },
    /// Взвести слежение за физическим извлечением носителя.
    WatchEject {
        on_eject: Box<dyn FnOnce() + Send + 'static>,
        mountpoint: String,
    },
}

/// Клиент актора: реализует `BlockSource`, но каждый вызов уходит в поток-актор.
pub struct BlockSourceClient {
    req_tx: Sender<Request>,
    block_size: usize,
    block_count: u64,
}

impl BlockSource for BlockSourceClient {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&mut self, lba: u64) -> io::Result<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        self.req_tx
            .send(Request::ReadBlock { lba, resp: tx })
            .map_err(|_| broken("actor thread is gone"))?;
        rx.recv().map_err(|_| broken("actor dropped the reply"))?
    }
}

struct DeviceActor<B: BlockSource> {
    inner: B,
    spt: u64,
    block_size: usize,
    block_count: u64,
    /// track index -> все секторы дорожки.
    cache: HashMap<u64, Vec<Vec<u8>>>,
    eject_watch: Option<(Box<dyn FnOnce() + Send + 'static>, String)>,
}

impl<B: BlockSource> DeviceActor<B> {
    fn run(mut self, rx: Receiver<Request>) {
        // Цикл завершится сам, когда исчезнут все клиенты (Sender'ы) → размонтирование.
        for req in rx {
            match req {
                Request::ReadBlock { lba, resp } => {
                    // Ловим панику декодера/протокола: одна битая дорожка не должна
                    // ронять весь актор (иначе весь том отваливается).
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.cached_read(lba)
                    }))
                    .unwrap_or_else(|_| {
                        Err(io::Error::new(io::ErrorKind::Other, "паника при чтении блока"))
                    });
                    self.check_eject(&r);
                    let _ = resp.send(r);
                }
                Request::WatchEject { on_eject, mountpoint } => {
                    self.eject_watch = Some((on_eject, mountpoint));
                }
            }
        }
    }

    /// Реактивное определение извлечения: привод без дискеты не даёт индекс-импульсов,
    /// и чтение возвращает `NotConnected` (NO_INDEX). DSKCHG на этом железе нечитаем,
    /// поэтому ловим извлечение по этому признаку при ближайшем чтении. При срабатывании —
    /// сообщение и размонтирование (том исчезает, `Session::run` в `main` завершается).
    fn check_eject(&mut self, r: &io::Result<Vec<u8>>) {
        let ejected = matches!(r, Err(e) if e.kind() == io::ErrorKind::NotConnected);
        if !ejected {
            return;
        }
        if let Some((on_eject, mountpoint)) = self.eject_watch.take() {
            println!("Дискета извлечена — размонтирую {mountpoint} и выхожу.");
            on_eject();
        }
    }

    fn cached_read(&mut self, lba: u64) -> io::Result<Vec<u8>> {
        let track = lba / self.spt;
        if !self.cache.contains_key(&track) {
            let sectors = self.read_track(track)?;
            log::debug!("track {track}: miss (прочитано {} секторов)", sectors.len());
            self.cache.insert(track, sectors);
        } else {
            log::debug!("track {track}: hit");
        }
        let idx = (lba % self.spt) as usize;
        Ok(self.cache[&track][idx].clone())
    }

    /// Прочитать все `spt` секторов дорожки. За концом носителя — нули.
    fn read_track(&mut self, track: u64) -> io::Result<Vec<Vec<u8>>> {
        let base = track * self.spt;
        let mut sectors = Vec::with_capacity(self.spt as usize);
        for i in 0..self.spt {
            let lba = base + i;
            if lba >= self.block_count {
                sectors.push(vec![0u8; self.block_size]);
            } else {
                sectors.push(self.inner.read_block(lba)?);
            }
        }
        Ok(sectors)
    }
}

/// Ручка управления актором со стороны `main`: взводит слежение за извлечением
/// носителя и позволяет дождаться завершения потока (чтобы `Drop` источника —
/// гашение мотора — успел отработать до выхода процесса).
pub struct ActorHandle {
    arm_tx: Sender<Request>,
    join: thread::JoinHandle<()>,
}

impl ActorHandle {
    /// Взвести слежение за физическим извлечением носителя.
    pub fn watch_eject(&self, on_eject: Box<dyn FnOnce() + Send + 'static>, mountpoint: String) {
        let _ = self.arm_tx.send(Request::WatchEject { on_eject, mountpoint });
    }

    /// Дождаться завершения актора. Сначала дропаем свой `Sender`, иначе актор
    /// никогда не отсоединится (останется живой отправитель) и join зависнет.
    pub fn join(self) {
        drop(self.arm_tx);
        let _ = self.join.join();
    }
}

/// Запустить актор над источником и вернуть клиент-`BlockSource` и ручку.
///
/// `spt` — число секторов на дорожку (гранула кэша). Для PC-дискет — 18 (1.44МБ)
/// или 9 (720КБ); на шаге 3 уточнится из BPB для совпадения с физдорожками.
pub fn spawn<B: BlockSource + 'static>(inner: B, spt: u64) -> (BlockSourceClient, ActorHandle) {
    let block_size = inner.block_size();
    let block_count = inner.block_count();
    let (req_tx, rx) = mpsc::channel::<Request>();

    let join = thread::Builder::new()
        .name("floppy-device-actor".into())
        .spawn(move || {
            let actor = DeviceActor {
                inner,
                spt: spt.max(1),
                block_size,
                block_count,
                cache: HashMap::new(),
                eject_watch: None,
            };
            actor.run(rx);
        })
        .expect("spawn device actor thread");

    let client = BlockSourceClient {
        req_tx: req_tx.clone(),
        block_size,
        block_count,
    };
    let handle = ActorHandle { arm_tx: req_tx, join };
    (client, handle)
}

fn broken(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Источник в памяти: каждый сектор помечен своим LBA в первом байте,
    /// а число обращений считается общим счётчиком (для проверки кэша).
    struct FakeSource {
        block_size: usize,
        block_count: u64,
        reads: Arc<AtomicUsize>,
    }

    impl BlockSource for FakeSource {
        fn block_size(&self) -> usize {
            self.block_size
        }
        fn block_count(&self) -> u64 {
            self.block_count
        }
        fn read_block(&mut self, lba: u64) -> io::Result<Vec<u8>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let mut b = vec![0u8; self.block_size];
            b[0] = lba as u8;
            Ok(b)
        }
    }

    #[test]
    fn track_is_read_once_and_cached() {
        let reads = Arc::new(AtomicUsize::new(0));
        let src = FakeSource {
            block_size: 512,
            block_count: 36, // две дорожки по 18
            reads: reads.clone(),
        };
        let (mut client, _h) = spawn(src, 18);

        // Промах по дорожке 0 → ровно 18 физических чтений.
        let s0 = client.read_block(0).unwrap();
        assert_eq!(s0[0], 0);
        assert_eq!(reads.load(Ordering::SeqCst), 18);

        // Другой сектор той же дорожки → попадание, новых чтений нет.
        let s5 = client.read_block(5).unwrap();
        assert_eq!(s5[0], 5);
        assert_eq!(reads.load(Ordering::SeqCst), 18);

        // Соседняя дорожка → ещё 18 чтений.
        let s18 = client.read_block(18).unwrap();
        assert_eq!(s18[0], 18);
        assert_eq!(reads.load(Ordering::SeqCst), 36);
    }

    #[test]
    fn reads_past_end_are_zero_filled() {
        let reads = Arc::new(AtomicUsize::new(0));
        let src = FakeSource {
            block_size: 512,
            block_count: 10, // дорожка 0 частично за концом носителя
            reads: reads.clone(),
        };
        let (mut client, _h) = spawn(src, 18);
        let s10 = client.read_block(10).unwrap(); // за концом
        assert_eq!(s10, vec![0u8; 512]);
        // прочитаны только реально существующие секторы 0..10
        assert_eq!(reads.load(Ordering::SeqCst), 10);
    }
}
