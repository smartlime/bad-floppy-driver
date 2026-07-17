# Тестовые образы

`*.img` не хранятся в git (генерируемые). Чтобы создать чистый FAT12 1.44МБ
образ `test.img` с несколькими файлами:

```sh
# 1) нулевой носитель ровно 1.44МБ
dd if=/dev/zero of=tests/fixtures/test.img bs=512 count=2880

# 2) raw-attach → /dev-узел
DEV=$(hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage \
        tests/fixtures/test.img | awk 'NR==1{print $1}')

# 3) формат FAT12 и наполнение
newfs_msdos -F 12 -v FLOPPY "$DEV"
mkdir -p /tmp/img_write && mount -t msdos "$DEV" /tmp/img_write
echo "hello" > /tmp/img_write/HELLO.TXT
mkdir /tmp/img_write/DOCS && echo "nested" > /tmp/img_write/DOCS/READ.ME
sync && umount /tmp/img_write && hdiutil detach "$DEV"
```

Проверка драйвером:

```sh
cargo build
./target/debug/floppy_mac /tmp/floppy_mnt --image tests/fixtures/test.img &
ls -laR /tmp/floppy_mnt
umount /tmp/floppy_mnt
```

Свой реальный дамп дискеты (сырой линейный `.img`/`.ima`, обычно 1 474 560 байт)
можно положить сюда и смонтировать тем же `--image`.
