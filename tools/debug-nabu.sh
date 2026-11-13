#!/usr/bin/env bash
# debug-nabu.sh -- overlay a diagnostic layer onto an already-built nabu boot
# image so a black screen can still be diagnosed over adb.
#
# What it adds (nothing else is changed):
#   * a logger shell that wraps /system/bin/recovery and writes, on every boot,
#       - /tmp/dmesg.log      : the full kernel ring buffer (dmesg -w), so the
#                               boot log up to the failure is captured
#       - /tmp/skyward.log    : Skyward's stdout/stderr (the DRM/panic errors)
#       - /tmp/pstore.log     : any prior-boot kernel panic captured by ramoops
#   * an explicit guarantee that adbd is started (sys.usb.config=adb) so the
#     device is reachable over USB even when the UI never draws.
#
# Usage:
#   tools/debug-nabu.sh --boot path/to/boot.img --out outdir/
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HELPER="$REPO_ROOT/tools/nabu_bootimg.py"

BOOT=""
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --boot)   BOOT="$2"; shift 2;;
    --out)    OUT="$2";  shift 2;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done
[ -n "$BOOT" ] || { echo "--boot is required" >&2; exit 2; }
[ -n "$OUT"  ] || { echo "--out is required"  >&2; exit 2; }
command -v xz   >/dev/null || { echo "xz is required"   >&2; exit 1; }
command -v cpio >/dev/null || { echo "cpio is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }

WORK="$(mktemp -d)"
RD="$WORK/rd"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

echo "extracting $(basename "$BOOT")..."
NABU_WORK="$WORK" python3 "$HELPER" extract "$BOOT" "$WORK" >/dev/null

echo "decompressing ramdisk (LZMA1)..."
xz --format=lzma -dc "$WORK/ramdisk.bin" > "$WORK/ramdisk.cpio"
mkdir -p "$RD"
( cd "$RD" && cpio -idm < "$WORK/ramdisk.cpio" )

echo "adding diagnostic layer..."

# 1) the logger that wraps recovery
cat > "$RD/system/bin/recovery-logger.sh" <<'SH'
#!/system/bin/sh
# Diagnostic wrapper: keep the kernel log and Skyward's output on /tmp so a
# black screen can be pulled over adb with:  adb pull /tmp
# Best-effort capture of a previous-boot panic left by ramoops.
mkdir -p /tmp
cp /sys/fs/pstore/* /tmp/pstore.log 2>/dev/null || true
# Run dmesg in follow mode first so the whole boot buffer is saved, then keep
# streaming.  setsid keeps it alive after this script's foreground child exits.
setsid dmesg -w >> /tmp/dmesg.log 2>&1 &
exec /system/bin/recovery >> /tmp/skyward.log 2>&1
SH
chmod 0755 "$RD/system/bin/recovery-logger.sh"

# 2) point the recovery service at the wrapper (keep its socket/seclabel/user)
#    init.recovery.service.rc:  service recovery /system/bin/recovery
sed -i 's#^service recovery /system/bin/recovery$#service recovery /system/bin/sh /system/bin/recovery-logger.sh#' "$RD/init.recovery.service.rc"
grep -n "service recovery" "$RD/init.recovery.service.rc"

# 3) explicit adbd guarantee + tmpfs safety, imported by init.rc
cat > "$RD/init.recovery.debug.rc" <<'RC'
on boot
    # Make absolutely sure the device is reachable over USB even if the UI
    # never draws, so /tmp logs can be pulled.
    setprop sys.usb.configfs 1
    setprop sys.usb.config adb
    start adbd
RC
# insert the import right after the qcom hardware rc import
sed -i 's#^import /init.recovery.${ro.hardware}.rc$#import /init.recovery.${ro.hardware}.rc\nimport /init.recovery.debug.rc#' "$RD/system/etc/init/hw/init.rc"
grep -n "init.recovery.debug.rc" "$RD/system/etc/init/hw/init.rc"

echo "repacking ramdisk (LZMA1, original format)..."
( cd "$RD" && find . -print0 | cpio -0 -o -H newc ) > "$WORK/ramdisk.new.cpio"
xz --format=lzma -9e < "$WORK/ramdisk.new.cpio" > "$WORK/ramdisk.new.lzma"

echo "reassembling boot image..."
NABU_WORK="$WORK" python3 "$HELPER" reassemble "$BOOT" "$WORK/ramdisk.new.lzma" "$OUT"

echo "done -> $OUT/$(basename "$BOOT")"
