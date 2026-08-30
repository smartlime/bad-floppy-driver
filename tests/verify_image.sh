#!/usr/bin/env bash
# Сравнить содержимое образа .img с эталонной директорией.
#
# Использование:
#   ./tests/verify_image.sh disk.img /Volumes/NO\ NAME
#
# Альтернатива через --verify флаг бинарника:
#   cargo run --release -- --verify /Volumes/NO\ NAME --image disk.img
#
# Скрипт делает то же самое, но через hdiutil (монтирует образ) и diff -r,
# что позволяет сравнить независимо от нашего кода чтения.
#
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "Использование: $0 <образ.img> <эталонная_директория>"
    echo "Пример: $0 disk.img /Volumes/NO\\ NAME"
    exit 1
fi

IMAGE="$1"
REF_DIR="$2"

if [[ ! -f "$IMAGE" ]]; then
    echo "Файл не найден: $IMAGE" >&2
    exit 1
fi
if [[ ! -d "$REF_DIR" ]]; then
    echo "Директория не найдена: $REF_DIR" >&2
    exit 1
fi

MOUNTPOINT=$(mktemp -d)
cleanup() {
    hdiutil detach "$MOUNTPOINT" -quiet 2>/dev/null || true
    rmdir "$MOUNTPOINT" 2>/dev/null || true
}
trap cleanup EXIT

echo "Монтирую образ $IMAGE в $MOUNTPOINT …"
hdiutil attach -readonly -nobrowse -mountpoint "$MOUNTPOINT" "$IMAGE"

echo "Сравниваю с эталоном $REF_DIR …"
# --exclude .metadata_never_index -- виртуальный файл нашего драйвера
if diff -r --exclude=".metadata_never_index" --exclude="Icon?" \
        "$MOUNTPOINT" "$REF_DIR"; then
    echo "OK: образ совпадает с эталоном."
    exit 0
else
    echo "FAIL: есть различия." >&2
    exit 1
fi
