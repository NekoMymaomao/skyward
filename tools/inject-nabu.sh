#!/usr/bin/env bash
# Build a Skyward recovery for Xiaomi Pad 5 (nabu) by hand.
#
# nabu has no recovery partition: its recovery is the ramdisk of boot, and an
# update rewrites boot, vendor_boot and dtbo together, so the three are flashed
# and kept as one boot set. This script does not use skyward-pack. It keeps the
# OrangeFox R11 base whole -- its init, adbd, SELinux policy, FBE decrypt
# scripts and the panel driver stay -- and only drops Skyward in:
#
#   * system/bin/recovery      -> the Skyward binary you built
#   * system/bin/7za,avbctl,resetprop,unzip,zstd -> links to recovery
#   * skyward/system-init      -> the statically linked first program (optional)
#   * skyward/device.toml      -> devices/nabu.toml
#   * skyward/MiSansVF.ttf, skyward/KirkMono-Regular.ttf, skyward/KirkMono-OFL.txt
#   * system/etc/task_profiles.json  -> only if the base lacks one
#   * system/etc/init/skyward-mtp.rc
#   * prop.default             -> ro.secure=0, ro.adb.secure=0, ro.debuggable=1,
#                                 ro.ota.allow_downgrade=true (build versions out)
#
# The ramdisk is repacked in the same LZMA1 framing the 4.14 kernel unpacks, so
# the kernel needs no change. The boot image is reassembled by tools/nabu_bootimg.py.
#
# Usage:
#   tools/inject-nabu.sh \
#       --skyward path/to/skyward \
#       --config  devices/nabu.toml \
#       --boot    path/to/orangefox-boot.img \
#       --vendor-boot path/to/vendor_boot.img \
#       --dtbo    path/to/dtbo.img \
#       --out     out/
#   [--system-init path/to/system-init]

set -euo pipefail

SKYWARD= SYSTEM_INIT= CONFIG= BOOT= VENDOR_BOOT= DTBO= OUT=
while [ $# -gt 0 ]; do
    case "$1" in
        --skyward)      SKYWARD="$2"; shift 2 ;;
        --system-init)  SYSTEM_INIT="$2"; shift 2 ;;
        --config)       CONFIG="$2"; shift 2 ;;
        --boot)         BOOT="$2"; shift 2 ;;
        --vendor-boot)  VENDOR_BOOT="$2"; shift 2 ;;
        --dtbo)         DTBO="$2"; shift 2 ;;
        --out)          OUT="$2"; shift 2 ;;
        -h|--help)      sed -n '2,42p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

for v in SKYWARD CONFIG BOOT VENDOR_BOOT DTBO OUT; do
    if [ -z "${!v}" ]; then echo "missing --${v,,}" >&2; exit 2; fi
done
for f in "$SKYWARD" "$CONFIG" "$BOOT" "$VENDOR_BOOT" "$DTBO"; do
    [ -f "$f" ] || { echo "no such file: $f" >&2; exit 1; }
done

command -v xz >/dev/null || { echo "xz is required" >&2; exit 1; }
command -v cpio >/dev/null || { echo "cpio is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HELPER="$REPO_ROOT/tools/nabu_bootimg.py"
FONTS="$REPO_ROOT/resources/fonts"
MTP_RC="$REPO_ROOT/crates/skyward-pack/src/mtp.rc"
TASK_PROFILES="$REPO_ROOT/crates/skyward-pack/src/task_profiles.json"
[ -f "$HELPER" ] || { echo "no helper at $HELPER" >&2; exit 1; }
[ -f "$FONTS/MiSansVF.ttf" ] || { echo "no font at $FONTS/MiSansVF.ttf" >&2; exit 1; }
[ -f "$MTP_RC" ] || { echo "no mtp.rc at $MTP_RC" >&2; exit 1; }
[ -f "$TASK_PROFILES" ] || { echo "no task_profiles.json at $TASK_PROFILES" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
RD="$WORK/rd"

echo "unpacking the boot image ramdisk..."
NABU_WORK="$WORK" python3 "$HELPER" extract "$BOOT" "$WORK"

echo "decompressing the LZMA1 ramdisk..."
xz --format=lzma -dc "$WORK/ramdisk.bin" > "$WORK/ramdisk.cpio"
mkdir -p "$RD"
( cd "$RD" && cpio -idm < "$WORK/ramdisk.cpio" )

echo "overwriting system/bin/recovery with Skyward..."
mkdir -p "$RD/system/bin"
cp "$SKYWARD" "$RD/system/bin/recovery"
chmod 0755 "$RD/system/bin/recovery"

echo "linking the commands install scripts ask for to the recovery binary..."
for cmd in 7za avbctl resetprop unzip zstd; do
    ln -sf recovery "$RD/system/bin/$cmd"
done

echo "adding Skyward assets to the ramdisk..."
mkdir -p "$RD/skyward" "$RD/system/etc/init"
cp "$CONFIG"        "$RD/skyward/device.toml"
cp "$FONTS/MiSansVF.ttf"        "$RD/skyward/MiSansVF.ttf"
cp "$FONTS/KirkMono-Regular.ttf" "$RD/skyward/KirkMono-Regular.ttf"
cp "$FONTS/KirkMono-OFL.txt"     "$RD/skyward/KirkMono-OFL.txt"
if [ -n "$SYSTEM_INIT" ]; then
    cp "$SYSTEM_INIT" "$RD/skyward/system-init"
    chmod 0755 "$RD/skyward/system-init"
fi
[ -e "$RD/system/etc/task_profiles.json" ] || cp "$TASK_PROFILES" "$RD/system/etc/task_profiles.json"
cp "$MTP_RC" "$RD/system/etc/init/skyward-mtp.rc"

echo "patching prop.default..."
python3 - "$RD/prop.default" <<PY
import os, sys
path = sys.argv[1]
if not os.path.exists(path):
    sys.exit(path + " is missing from the ramdisk")
text = open(path, "rb").read()
drop = (b"ro.build.version.release", b"ro.build.version.security_patch", b"ro.vendor.build.security_patch")
out = b"".join(
    line for line in text.splitlines(keepends=True)
    if line.split(b"=")[0].strip() not in drop
)
add = b"\nro.secure=0\nro.adb.secure=0\nro.debuggable=1\nro.ota.allow_downgrade=true\n"
open(path, "wb").write(out + add)
PY

echo "repacking the ramdisk as LZMA1..."
( cd "$RD" && find . -print0 | cpio -0 -o -H newc ) > "$WORK/ramdisk.new.cpio"
xz --format=lzma -9e < "$WORK/ramdisk.new.cpio" > "$WORK/ramdisk.new.lzma"

echo "reassembling boot.img..."
NABU_WORK="$WORK" python3 "$HELPER" reassemble "$BOOT" "$WORK/ramdisk.new.lzma" "$OUT"

echo "copying vendor_boot and dtbo alongside the boot image..."
cp "$VENDOR_BOOT" "$OUT/$(basename "$VENDOR_BOOT")"
cp "$DTBO" "$OUT/$(basename "$DTBO")"

echo
echo "done. Flash the three images, one slot at a time:"
echo "  fastboot flash boot_a        $OUT/$(basename "$BOOT")"
echo "  fastboot flash vendor_boot_a $OUT/$(basename "$VENDOR_BOOT")"
echo "  fastboot flash dtbo_a        $OUT/$(basename "$DTBO")"
echo "  fastboot reboot recovery"
