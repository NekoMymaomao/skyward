#!/usr/bin/env python3
# Boot image (header v3) helpers for tools/inject-nabu.sh: the kernel and
# ramdisk are pulled out of an OrangeFox boot image, and put back once the
# ramdisk has Skyward dropped in. The header is kept whole and only its
# ramdisk size is patched, so the rebuilt image is a drop-in for fastboot.
import os
import struct
import sys

PAGE = 4096


def extract(boot_path, work):
    with open(boot_path, "rb") as f:
        header = f.read(4096)
        if header[:8] != b"ANDROID!":
            sys.exit("not an Android boot image")
        kernel_size, ramdisk_size = struct.unpack("<II", header[8:16])
        (header_version,) = struct.unpack("<I", header[40:44])
        if header_version < 3:
            sys.exit("boot header v%d lacks the simple kernel+ramdisk layout" % header_version)
        kernel_off = PAGE
        ramdisk_off = kernel_off + ((kernel_size + PAGE - 1) // PAGE) * PAGE
        f.seek(kernel_off)
        kernel = f.read(kernel_size)
        f.seek(ramdisk_off)
        ramdisk = f.read(ramdisk_size)
    with open(os.path.join(work, "kernel.bin"), "wb") as k:
        k.write(kernel)
    with open(os.path.join(work, "ramdisk.bin"), "wb") as r:
        r.write(ramdisk)
    with open(os.path.join(work, "header.bin"), "wb") as h:
        h.write(header)
    with open(os.path.join(work, "partsize"), "w") as s:
        s.write(str(os.path.getsize(boot_path)))
    print("kernel %d bytes, ramdisk %d bytes, header v%d" % (kernel_size, ramdisk_size, header_version))


def reassemble(work, boot_path, ramdisk_path, out_dir):
    header = bytearray(open(os.path.join(work, "header.bin"), "rb").read())
    kernel = open(os.path.join(work, "kernel.bin"), "rb").read()
    ramdisk = open(ramdisk_path, "rb").read()
    struct.pack_into("<I", header, 0x0C, len(ramdisk))
    pad = lambda b: b + b"\0" * ((-len(b)) % PAGE)
    body = pad(kernel) + pad(ramdisk)
    part = int(open(os.path.join(work, "partsize")).read())
    # The header is the first page of the partition itself, so the kernel and
    # ramdisk only fill what is left of it; padding to `part - PAGE` makes the
    # whole image exactly the partition size rather than a page too large.
    if len(body) > part - PAGE:
        sys.exit("the new ramdisk overruns the %d-byte partition" % part)
    body = body + b"\0" * (part - PAGE - len(body))
    os.makedirs(out_dir, exist_ok=True)
    name = os.path.basename(boot_path)
    with open(os.path.join(out_dir, name), "wb") as f:
        f.write(bytes(header) + body)
    print("wrote %s: %d kernel + %d ramdisk, padded to %d" % (name, len(kernel), len(ramdisk), part))


if __name__ == "__main__":
    command, *rest = sys.argv[1:]
    work = os.environ["NABU_WORK"]
    if command == "extract":
        extract(rest[0], rest[1])
    elif command == "reassemble":
        reassemble(work, rest[0], rest[1], rest[2])
    else:
        sys.exit("unknown command: %s" % command)
