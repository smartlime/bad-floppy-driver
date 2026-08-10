# floppy-mac

Read-only macOS FUSE-драйвер для монтирования физических дискет прямо в Finder через [Greaseweazle](https://github.com/keirf/greaseweazle). Написан на Rust: флукс с адаптера → собственный MFM-декодер → FAT / TR-DOS → macFUSE. Работает без kext, без Xcode, на Full Security (Apple Silicon). Поддерживает живые приводы через Greaseweazle и готовые образы `.img`.

Форматы: MS-DOS FAT12/16 и TR-DOS (ZX Spectrum). Формат определяется автоматически. Если сторона 0 не читается (однобокая TR-DOS дискета), автоматически берётся сторона 1.

→ [Установка](#установка)

---

## Использование

```sh
# Найти порт Greaseweazle
floppy_mac --list-devices
#   → /dev/tty.usbmodem11201

# Смонтировать дискету
floppy_mac /tmp/floppy --device /dev/tty.usbmodem11201

# Открыть в Finder
open /tmp/floppy

# Размонтировать
umount /tmp/floppy
# или Ctrl+C в окне драйвера
```

Смонтировать готовый образ `.img` (без железа):

```sh
floppy_mac /tmp/floppy --image disk.img
```

Плохая дискета — читать несмотря на битые секторы (нулевые заглушки вместо ошибок):

```sh
floppy_mac /tmp/floppy --device /dev/tty.usbmodem11201 --recover
```

Диагностика без монтирования — прошивка, дорожка 0, геометрия:

```sh
floppy_mac --probe --device /dev/tty.usbmodem11201
```

## Опции

```
floppy_mac <mountpoint> [--device <port> | --image <file>] [--unit 0|1|A|B] [options]
floppy_mac --list-devices
```

| Аргумент              | Описание                                                                              |
|-----------------------|---------------------------------------------------------------------------------------|
| `<mountpoint>`        | Точка монтирования                                                                    |
| `--device <port>`     | Живая дискета через Greaseweazle на указанном serial-порту                            |
| `--image <file>`      | Смонтировать готовый образ `.img`                                                     |
| `--list-devices`      | Показать доступные serial-порты и выйти                                               |
| `--probe`             | Диагностика чтения без монтирования (нужен `--device`)                                |
| `--recover`           | Best-effort: битые секторы — нулями, без ошибок I/O; в конце — отчёт о повреждениях  |
| `--unit <n>`          | Привод: `0`–`3` (Shugart/GW F1 Plus) или `A`/`B` (IBM PC шина);  0 ≠ A! Тип шины определяется автоматически |
| `--hd`                | HD-режим: Сигнал density-select низкого уровня, нужен для 5.25″ HD приводов |
| `--revs <n>`          | Оборотов на чтение дорожки (по умолчанию `3`; больше — надёжнее для слабых дискет)   |
| `-v`, `--verbose`     | Подробный лог (команды GW, PLL, секторы, BPB) в stderr                               |

Детальный лог через переменную среды: `RUST_LOG=info` (геометрия, ретраи), `RUST_LOG=debug` (попадания кэша).

---

## Установка

### Через Homebrew (рекомендуется)

Сначала установите [macFUSE](https://macfuse.github.io/):

```sh
brew install --cask macfuse
```

Затем добавьте tap и установите:

```sh
brew tap smartlime/floppy-mac
brew install floppy-mac
```

### Готовый бинарник

Скачайте `floppy_mac` со страницы [Releases](https://github.com/smartlime/mac-floppy-driver/releases), сделайте исполняемым и положите куда-нибудь в `$PATH`:

```sh
chmod +x floppy_mac
sudo mv floppy_mac /usr/local/bin/
```

### Из исходников

Требования: Rust, [macFUSE 5+](https://macfuse.github.io/).

```sh
git clone https://github.com/smartlime/mac-floppy-driver
cd mac-floppy-driver
cargo build --release
# бинарник: target/release/floppy_mac
```

---

## Тесты

```sh
cargo test
```

13 unit-тестов покрывают: MFM-декодер (CRC, джиттер, полная дорожка 18 секторов), протокол Greaseweazle на уровне wire, кэширующий актор, разбор FAT BPB, конвертацию времени FAT.

---

## Протестировано

На Greaseweazle F1 rev.1 с 3.5″ дисководом (1.44 МБ, MS-DOS FAT12), macOS Sequoia / Apple Silicon. Другие ревизии GW (F7, HD и пр.) используют тот же протокол v1.x — должны работать без изменений.

---

## Известные допущения и ограничения

- Только чтение. Запись не реализована.
- Форматы: FAT12/FAT16 (MS-DOS) и TR-DOS (ZX Spectrum, 256 B/сектор). Amiga OFS/FFS, CP/M и другие — вне скоупа.
- Авто-детект вставки дискеты не реализован: монтирование — явной командой.
- При извлечении дискеты во время монтирования том размонтируется автоматически.
- Тестировалось только с 3.5″ 1.44 МБ. 5.25″ и другие форматы теоретически поддерживаются через тот же путь, но не проверялись.
- Требуется [macFUSE 5+](https://macfuse.github.io/) (устанавливается через brew cask или dmg со страницы проекта).

---

## Использованные библиотеки

| Крейт | Назначение |
|---|---|
| [fuser](https://crates.io/crates/fuser) | macFUSE / FUSE bindings для Rust |
| [fatfs](https://crates.io/crates/fatfs) | Парсинг FAT12/FAT16/FAT32 |
| [serialport](https://crates.io/crates/serialport) | Serial-порт (USB CDC) для Greaseweazle |
| [log](https://crates.io/crates/log) + [env_logger](https://crates.io/crates/env_logger) | Логирование |
| [libc](https://crates.io/crates/libc) | POSIX-типы для FUSE |

Протокол Greaseweazle и MFM-декодер реализованы самостоятельно по официальному public-domain [`usb.py`](https://github.com/keirf/greaseweazle/blob/master/src/greaseweazle/usb.py) — стороннего кода в проекте нет.

---

## Лицензия

GPL-2.0-or-later (следствие линковки с macFUSE).

---

## Обратная связь

Замечания, баги и патчи — в [Issues](https://github.com/smartlime/mac-floppy-driver/issues) или в Telegram-канал [@varnakov_ru](https://t.me/varnakov_ru).
