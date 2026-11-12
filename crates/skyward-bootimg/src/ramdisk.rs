use anyhow::{bail, Context, Result};
use cpio::{NewcBuilder, NewcReader};
use std::ffi::{CString, OsStr};
use std::fs;
use std::io::{BufReader, Cursor, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};

/// How a boot image's ramdisk is compressed. Android's own images use LZ4
/// in legacy framing; some makers and custom recoveries use gzip or LZMA1
/// ("lzma alone") instead. The bootloader hands the ramdisk to the kernel
/// as it is, so an image must be written back in the framing it came in:
/// a 4.14 kernel may have been built to read only one of these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// LZ4 legacy framing, as AOSP packs boot ramdisks.
    #[default]
    Lz4Legacy,
    /// LZ4 frame format (the `lz4` tool's default, no `-l`).
    Lz4Frame,
    Gzip,
    /// LZMA1 in the `.lzma` ("alone") container.
    Lzma1,
    /// An uncompressed newc archive.
    Plain,
}

const LZ4_LEGACY: &[u8; 4] = b"\x02\x21\x4c\x18";
const LZ4_FRAME: &[u8; 4] = b"\x04\x22\x4d\x18";
const GZIP: &[u8; 2] = b"\x1f\x8b";
const NEWC: &[u8; 6] = b"070701";
const NEWC_CRC: &[u8; 6] = b"070702";

impl Compression {
    /// How `bytes` are compressed, by the framing's magic number.
    pub fn of(bytes: &[u8]) -> Result<Self> {
        if bytes.starts_with(LZ4_LEGACY) {
            return Ok(Self::Lz4Legacy);
        }
        if bytes.starts_with(LZ4_FRAME) {
            return Ok(Self::Lz4Frame);
        }
        if bytes.starts_with(GZIP) {
            return Ok(Self::Gzip);
        }
        if bytes.starts_with(NEWC) || bytes.starts_with(NEWC_CRC) {
            return Ok(Self::Plain);
        }
        // LZMA1 has no magic number: one properties byte, four of dictionary
        // size, eight of the unpacked size (often all 0xff when unknown).
        // Its properties byte is 0x5d for the settings every boot image
        // uses, and the byte after the dictionary size starts the size.
        if bytes.len() >= 13 && bytes[0] == 0x5d && bytes[5..13].iter().all(|b| *b == 0xff) {
            return Ok(Self::Lzma1);
        }
        bail!("the ramdisk starts with neither LZ4, gzip, LZMA1 nor a newc archive")
    }

    /// The uncompressed newc archive `bytes` hold.
    fn decompress(self, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            Self::Lz4Legacy | Self::Lz4Frame => {
                lz4_flex::frame::FrameDecoder::new(bytes).read_to_end(&mut out).context("lz4 decode")?;
            }
            Self::Gzip => {
                flate2::read::GzDecoder::new(bytes).read_to_end(&mut out).context("gzip decode")?;
            }
            Self::Lzma1 => {
                lzma_rs::lzma_decompress(&mut BufReader::new(bytes), &mut out).context("lzma decode")?;
            }
            Self::Plain => out.extend_from_slice(bytes),
        }
        Ok(out)
    }

    /// `archive` in this framing, to hand back to the kernel.
    fn compress(self, archive: &[u8]) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Lz4Legacy | Self::Lz4Frame => lz4_tool(archive)?,
            Self::Gzip => {
                let mut out = Vec::new();
                flate2::read::GzEncoder::new(archive, flate2::Compression::default()).read_to_end(&mut out)?;
                out
            }
            Self::Lzma1 => {
                let mut out = Vec::new();
                lzma_rs::lzma_compress(&mut BufReader::new(archive), &mut out).context("lzma compress")?;
                out
            }
            Self::Plain => archive.to_vec(),
        })
    }
}

/// One cpio entry with the metadata the kernel restores when it unpacks the
/// archive into rootfs.
#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
    pub nlink: u32,
    pub dev: (u32, u32),
    pub rdev: (u32, u32),
    pub data: Vec<u8>,
}

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;
const S_IFIFO: u32 = 0o010000;
const S_IFSOCK: u32 = 0o140000;

/// The permissions a stock ramdisk gives its links, such as the commands
/// in system/bin that stand for toybox.
const LINK_MODE: u32 = 0o755;

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

/// An initramfs held in memory, so entries keep their owners, modes and
/// device numbers exactly as the archive had them.
pub struct Ramdisk {
    pub entries: Vec<Entry>,
}

impl Ramdisk {
    /// Unpack a compressed newc archive in whichever framing it came in,
    /// remembering that framing for [`Ramdisk::encode_with`].
    pub fn decode_with(compressed: &[u8]) -> Result<(Self, Compression)> {
        let framing = Compression::of(compressed)?;
        Ok((Self::decode_as(compressed, framing)?, framing))
    }

    /// Unpack a compressed newc archive, as Android boot images carry. Its
    /// own framing is read from the bytes, LZ4 legacy framing by default.
    pub fn decode(compressed: &[u8]) -> Result<Self> {
        let framing = Compression::of(compressed)?;
        Self::decode_as(compressed, framing)
    }

    fn decode_as(compressed: &[u8], framing: Compression) -> Result<Self> {
        let cpio = framing.decompress(compressed)?;
        let mut entries = Vec::new();
        let mut rest = Cursor::new(cpio.as_slice());
        loop {
            let mut r = NewcReader::new(rest).context("cpio entry")?;
            if r.entry().is_trailer() {
                break;
            }
            let e = r.entry();
            let mut entry = Entry {
                name: e.name().to_string(),
                mode: e.mode(),
                uid: e.uid(),
                gid: e.gid(),
                mtime: e.mtime(),
                nlink: e.nlink(),
                dev: (e.dev_major(), e.dev_minor()),
                rdev: (e.rdev_major(), e.rdev_minor()),
                data: Vec::with_capacity(e.file_size() as usize),
            };
            r.read_to_end(&mut entry.data).with_context(|| format!("cpio data of {}", entry.name))?;
            entries.push(entry);
            rest = r.finish().context("cpio entry end")?;
        }
        Ok(Ramdisk { entries })
    }

    /// Pack as a newc archive compressed the way AOSP packs boot ramdisks,
    /// with the reference `lz4` tool in legacy framing (the kernel's initramfs
    /// unpacker reads no other LZ4 framing) at its highest compression.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.encode_with(Compression::Lz4Legacy)
    }

    /// Pack as a newc archive in the framing `framing` names, the one a
    /// stock image came in: the kernel of that device reads that framing.
    pub fn encode_with(&self, framing: Compression) -> Result<Vec<u8>> {
        framing.compress(&self.archive()?)
    }

    /// Take out every entry whose name is `prefix` or is under `prefix/`,
    /// as when a custom recovery's own tree is removed from the ramdisk a
    /// Skyward image is built on. Returns how many entries went.
    pub fn strip_prefixes(&mut self, prefixes: &[String]) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| {
            !prefixes.iter().any(|p| {
                let p = p.trim_end_matches('/');
                e.name == p || e.name.strip_prefix(p).is_some_and(|rest| rest.starts_with('/'))
            })
        });
        before - self.entries.len()
    }

    /// Pack as a newc archive, uncompressed.
    pub fn archive(&self) -> Result<Vec<u8>> {
        let mut cpio = Vec::new();
        for (ino, e) in self.entries.iter().enumerate() {
            let size = u32::try_from(e.data.len()).with_context(|| format!("{} is too large", e.name))?;
            let mut w = NewcBuilder::new(&e.name)
                .ino(300_000 + ino as u32)
                .mode(e.mode)
                .uid(e.uid)
                .gid(e.gid)
                .mtime(e.mtime)
                .nlink(e.nlink)
                .dev_major(e.dev.0)
                .dev_minor(e.dev.1)
                .rdev_major(e.rdev.0)
                .rdev_minor(e.rdev.1)
                .write(&mut cpio, size);
            w.write_all(&e.data)?;
            w.finish()?;
        }
        cpio::newc::trailer(&mut cpio)?;
        Ok(cpio)
    }

    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Unpack the archive over `root`, as the kernel unpacks each archive of
    /// an initramfs over those before it: an entry takes the place of what
    /// is there of another kind, a folder already there stays, and each
    /// entry gets the archive's owner, mode and time, a folder's time once
    /// everything in it is in. Where a file is already there, the entry is
    /// made anew, where the kernel writes over it, so that a program
    /// running from that file is left alone.
    pub fn unpack(&self, root: &Path) -> Result<()> {
        for e in &self.entries {
            let path = root.join(&e.name);
            unpack(e, &path).with_context(|| format!("unpack {}", path.display()))?;
        }
        for e in self.entries.iter().filter(|e| e.is_dir()) {
            let path = root.join(&e.name);
            set_time(&path, e.mtime).with_context(|| format!("unpack {}", path.display()))?;
        }
        Ok(())
    }

    /// Replace a regular file's contents, keeping its owner and mode.
    pub fn replace(&mut self, name: &str, data: Vec<u8>) -> Result<()> {
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(e) if e.mode & S_IFMT == S_IFREG => {
                e.data = data;
                Ok(())
            }
            Some(_) => bail!("{name} is not a regular file"),
            None => bail!("{name} is not in the ramdisk"),
        }
    }

    /// Add a root-owned regular file, creating missing parent directories.
    pub fn add(&mut self, name: &str, mode: u32, data: Vec<u8>) -> Result<()> {
        self.insert(name, S_IFREG | mode, data)
    }

    /// Point `name` to `target`: a link already there is pointed anew, and
    /// otherwise a root-owned link is added, creating missing parent
    /// directories.
    pub fn link(&mut self, name: &str, target: &str) -> Result<()> {
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(e) if e.mode & S_IFMT == S_IFLNK => {
                e.data = target.as_bytes().to_vec();
                Ok(())
            }
            Some(_) => bail!("{name} is in the ramdisk and is not a link"),
            None => self.insert(name, S_IFLNK | LINK_MODE, target.as_bytes().to_vec()),
        }
    }

    fn insert(&mut self, name: &str, mode: u32, data: Vec<u8>) -> Result<()> {
        if self.find(name).is_some() {
            bail!("{name} is already in the ramdisk");
        }
        let parts: Vec<&str> = name.split('/').collect();
        let mut parent = String::new();
        for part in &parts[..parts.len() - 1] {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(part);
            match self.find(&parent) {
                Some(e) if e.is_dir() => {}
                Some(_) => bail!("{parent} is not a directory"),
                None => self.entries.push(root_entry(&parent, S_IFDIR | 0o755, Vec::new())),
            }
        }
        self.entries.push(root_entry(name, mode, data));
        Ok(())
    }
}

fn root_entry(name: &str, mode: u32, data: Vec<u8>) -> Entry {
    let nlink = if mode & S_IFMT == S_IFDIR { 2 } else { 1 };
    Entry { name: name.to_string(), mode, uid: 0, gid: 0, mtime: 0, nlink, dev: (0, 0), rdev: (0, 0), data }
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("a NUL in the path")
}

/// One entry of [`Ramdisk::unpack`] at `path`.
fn unpack(e: &Entry, path: &Path) -> Result<()> {
    let kind = e.mode & S_IFMT;
    let mode = e.mode & 0o7777;
    let kept = match fs::symlink_metadata(path) {
        Ok(there) if there.is_dir() && kind == S_IFDIR => true,
        Ok(there) if there.is_dir() => {
            fs::remove_dir(path).context("take away the folder there")?;
            false
        }
        Ok(_) => {
            fs::remove_file(path).context("take away what is there")?;
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err).context("look at what is there"),
    };
    match kind {
        S_IFDIR if kept => {}
        S_IFDIR => fs::DirBuilder::new().mode(mode).create(path).context("make the folder")?,
        S_IFREG => {
            let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(path).context("make the file")?;
            file.write_all(&e.data).context("write the file")?;
        }
        S_IFLNK => std::os::unix::fs::symlink(OsStr::from_bytes(&e.data), path).context("make the link")?,
        S_IFCHR | S_IFBLK | S_IFIFO | S_IFSOCK => {
            let dev = libc::makedev(e.rdev.0 as _, e.rdev.1 as _);
            if unsafe { libc::mknod(c_path(path)?.as_ptr(), e.mode as libc::mode_t, dev) } != 0 {
                return Err(std::io::Error::last_os_error()).context("make the node");
            }
        }
        _ => bail!("mode {:o} is no kind of file", e.mode),
    }
    std::os::unix::fs::lchown(path, Some(e.uid), Some(e.gid)).context("give it its owner")?;
    // Taking an owner clears the set-ID bits, so the mode comes after; a
    // link has none of its own.
    if kind != S_IFLNK {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).context("give it its mode")?;
    }
    if kind != S_IFDIR {
        set_time(path, e.mtime)?;
    }
    Ok(())
}

/// Give `path` itself, a link or not, `mtime` as the time it was last
/// changed and read.
fn set_time(path: &Path, mtime: u32) -> Result<()> {
    let at = libc::timespec { tv_sec: mtime as libc::time_t, tv_nsec: 0 };
    let times = [at, at];
    if unsafe { libc::utimensat(libc::AT_FDCWD, c_path(path)?.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error()).context("give it its time");
    }
    Ok(())
}

/// The legacy LZ4 framing's magic number.
const LZ4_LEGACY_MAGIC: u32 = 0x184C_2102;
/// The most a block of the legacy framing unpacks to; the kernel unpacks no
/// more of one.
const LZ4_LEGACY_BLOCK: usize = 8 << 20;

/// `input` compressed in LZ4's legacy framing, the one the kernel's initramfs
/// unpacker reads: the magic number, then each 8 MiB of `input` as one LZ4
/// block with its compressed length before it. The kernel reads the ramdisks
/// the bootloader joins as one such stream, going on from one into the next
/// only where that one starts with the magic number. Compressed in process,
/// with no `lz4` tool, as the recovery does on the phone.
pub fn lz4_legacy(input: &[u8]) -> Vec<u8> {
    let mut out = LZ4_LEGACY_MAGIC.to_le_bytes().to_vec();
    for chunk in input.chunks(LZ4_LEGACY_BLOCK) {
        let block = lz4_flex::block::compress(chunk);
        let len = u32::try_from(block.len()).expect("8 MiB compresses to less than 4 GiB");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&block);
    }
    out
}

fn lz4_tool(input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("lz4")
        .args(["-l", "-12", "--favor-decSpeed", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("run lz4 (install it, e.g. `brew install lz4`)")?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().context("lz4")?;
    writer.join().expect("lz4 writer thread").context("feed lz4")?;
    if !out.status.success() {
        bail!("lz4 exited with {}", out.status);
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ramdisk in each framing reads back the same, and is written back
    /// in the framing it came in, so the device's kernel can still read it.
    #[test]
    fn a_ramdisk_is_read_and_written_back_in_its_own_framing() {
        let mut rd = Ramdisk { entries: Vec::new() };
        rd.add("system/bin/recovery", 0o750, b"binary binary binary".to_vec()).unwrap();
        rd.add("init", 0o755, b"#!/init\n".to_vec()).unwrap();
        // A frame-format image is read but always written back in legacy
        // framing, the only LZ4 framing the kernel's unpacker reads.
        for framing in [Compression::Lz4Legacy, Compression::Gzip, Compression::Lzma1, Compression::Plain] {
            let packed = rd.encode_with(framing).unwrap();
            let (back, found) = Ramdisk::decode_with(&packed).unwrap();
            assert_eq!(found, framing, "the framing of a {framing:?} ramdisk is read back");
            let names: Vec<&str> = back.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, rd.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), "a {framing:?} ramdisk keeps its entries");
            assert_eq!(back.find("system/bin/recovery").unwrap().data, b"binary binary binary");
        }
        // The `lz4` tool's default frame format is read all the same.
        let frame = lz4_tool_frame(&rd.archive().unwrap());
        assert_eq!(Compression::of(&frame).unwrap(), Compression::Lz4Frame);
        assert_eq!(Ramdisk::decode(&frame).unwrap().find("init").unwrap().data, b"#!/init\n");
    }

    fn lz4_tool_frame(archive: &[u8]) -> Vec<u8> {
        let mut child = Command::new("lz4").args(["-12", "-c"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = archive.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(out.status.success());
        out.stdout
    }

    /// The framing of the Xiaomi Pad 5's OrangeFox ramdisk: LZMA1, with the
    /// unpacked size left unknown rather than a magic number to go by.
    #[test]
    fn an_lzma_ramdisk_with_an_unknown_size_is_read() {
        let mut rd = Ramdisk { entries: Vec::new() };
        rd.add("init", 0o755, b"program".to_vec()).unwrap();
        let packed = rd.encode_with(Compression::Lzma1).unwrap();
        assert_eq!(packed[0], 0x5d, "the LZMA1 properties byte");
        assert_eq!(Compression::of(&packed).unwrap(), Compression::Lzma1);
        assert_eq!(Ramdisk::decode(&packed).unwrap().find("init").unwrap().data, b"program");
    }

    /// A custom recovery's own tree is taken out, with the rest of the
    /// ramdisk left alone.
    #[test]
    fn stripping_a_prefix_leaves_the_rest() {
        let mut rd = Ramdisk { entries: Vec::new() };
        rd.add("twres/ui.xml", 0o644, b"theme".to_vec()).unwrap();
        rd.add("FFiles/fox.mkshrc", 0o644, b"script".to_vec()).unwrap();
        rd.add("init", 0o755, b"program".to_vec()).unwrap();
        rd.add("twresources/keep", 0o644, b"not under twres".to_vec()).unwrap();
        // A folder's own entry goes with the tree under it, so four go:
        // twres, twres/ui.xml, FFiles, FFiles/fox.mkshrc.
        let gone = rd.strip_prefixes(&["twres".into(), "FFiles/".into()]);
        assert_eq!(gone, 4);
        let names: Vec<&str> = rd.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"twres/ui.xml") && !names.contains(&"twres"));
        assert!(!names.contains(&"FFiles/fox.mkshrc") && !names.contains(&"FFiles"));
        assert!(names.contains(&"init"), "the stock programs stay");
        assert!(names.contains(&"twresources/keep"), "a name that merely starts alike stays");
    }

    #[test]
    fn round_trip_keeps_metadata() {
        let mut rd = Ramdisk { entries: Vec::new() };
        rd.add("system/bin/recovery", 0o750, b"binary".to_vec()).unwrap();
        rd.entries.push(Entry {
            name: "dev/console".into(),
            mode: 0o020600,
            uid: 0,
            gid: 0,
            mtime: 0,
            nlink: 1,
            dev: (0, 0),
            rdev: (5, 1),
            data: Vec::new(),
        });
        let back = Ramdisk::decode(&rd.encode().unwrap()).unwrap();
        let names: Vec<&str> = back.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["system", "system/bin", "system/bin/recovery", "dev/console"]);
        let bin = back.find("system/bin/recovery").unwrap();
        assert_eq!((bin.mode, bin.data.as_slice()), (S_IFREG | 0o750, b"binary".as_slice()));
        assert_eq!(back.find("dev/console").unwrap().rdev, (5, 1));
        assert!(rd.add("system/bin/recovery", 0o644, Vec::new()).is_err());
    }

    /// What the reference `lz4` tool unpacks `packed` to.
    fn unpacked_by_the_tool(packed: &[u8]) -> Vec<u8> {
        let mut child = Command::new("lz4").args(["-d", "-c"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = packed.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(out.status.success(), "lz4 -d exited with {}", out.status);
        out.stdout
    }

    #[test]
    fn a_legacy_stream_starts_with_its_magic_and_unpacks_whole() {
        // Past one block of 8 MiB, compressible in places.
        let input: Vec<u8> = (0..20u32 << 20).map(|i| if i % 3 == 0 { (i / 7) as u8 } else { 0x5a }).collect();
        let packed = lz4_legacy(&input);
        assert_eq!(packed[..4], [0x02, 0x21, 0x4c, 0x18]);
        assert!(packed.len() < input.len());
        assert_eq!(unpacked_by_the_tool(&packed), input, "the reference tool reads it");
        let mut back = Vec::new();
        lz4_flex::frame::FrameDecoder::new(packed.as_slice()).read_to_end(&mut back).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn a_stream_after_the_vendor_ramdisk_is_read_on_into() {
        let mut vendor = Ramdisk { entries: Vec::new() };
        vendor.add("lib/modules/ufs_qcom.ko", 0o644, b"module".to_vec()).unwrap();
        let mut ours = Ramdisk { entries: Vec::new() };
        ours.add("init", 0o750, b"program".to_vec()).unwrap();
        let joined = [vendor.encode().unwrap(), lz4_legacy(&ours.archive().unwrap())].concat();
        assert_eq!(unpacked_by_the_tool(&joined), [vendor.archive().unwrap(), ours.archive().unwrap()].concat(), "both archives, one after the other");
    }

    #[test]
    fn a_link_is_kept_as_a_link_to_its_target() {
        let mut rd = Ramdisk { entries: Vec::new() };
        rd.add("system/bin/recovery", 0o750, b"binary".to_vec()).unwrap();
        rd.link("system/bin/zstd", "recovery").unwrap();
        rd.link("sbin/sh", "/system/bin/sh").unwrap();
        let back = Ramdisk::decode(&rd.encode().unwrap()).unwrap();
        let zstd = back.find("system/bin/zstd").unwrap();
        assert_eq!((zstd.mode, zstd.data.as_slice()), (S_IFLNK | LINK_MODE, b"recovery".as_slice()));
        assert_eq!((zstd.uid, zstd.gid, zstd.nlink), (0, 0, 1));
        assert!(back.find("sbin").is_some_and(Entry::is_dir), "a missing folder is made");
        assert!(rd.link("system/bin/recovery", "zstd").is_err(), "a file is no link to point anew");
        assert!(rd.link("system/bin/recovery/7za", "recovery").is_err(), "a file holds no links");
    }

    /// An entry owned by whoever runs the tests, who can give it that owner.
    fn mine(name: &str, mode: u32, data: &[u8]) -> Entry {
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Entry { name: name.into(), mode, uid, gid, mtime: 1_500_000_000, nlink: 1, dev: (0, 0), rdev: (0, 0), data: data.to_vec() }
    }

    #[test]
    fn unpacking_over_an_archive_unpacked_before_follows_the_kernel() {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // As the archives before it left the root.
        fs::write(root.join("init"), b"old program").unwrap();
        fs::create_dir(root.join("skyward")).unwrap();
        fs::write(root.join("skyward/system.toml"), b"id = \"test\"").unwrap();
        fs::DirBuilder::new().mode(0o700).create(root.join("dev")).unwrap();
        fs::write(root.join("etc"), b"a file").unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("bin")).unwrap();
        let rd = Ramdisk {
            entries: vec![
                mine("init", S_IFLNK | 0o750, b"/system/bin/init"),
                mine("dev", S_IFDIR | 0o755, b""),
                mine("etc", S_IFLNK | 0o644, b"/system/etc"),
                mine("bin", S_IFDIR | 0o751, b""),
                mine("skyward", S_IFDIR | 0o750, b""),
                mine("system", S_IFDIR | 0o755, b""),
                mine("system/bin", S_IFDIR | 0o755, b""),
                mine("system/bin/init", S_IFREG | 0o750, b"new program"),
                mine("system/bin/pipe", S_IFIFO | 0o600, b""),
            ],
        };
        rd.unpack(root).unwrap();
        let meta = |name: &str| fs::symlink_metadata(root.join(name)).unwrap();
        assert_eq!(fs::read_link(root.join("init")).unwrap(), Path::new("/system/bin/init"), "a link in the file's place");
        assert_eq!(fs::read(root.join("skyward/system.toml")).unwrap(), b"id = \"test\"", "what the archive leaves out stays");
        assert_eq!(meta("dev").permissions().mode() & 0o7777, 0o755, "a folder there stays, with the archive's mode");
        assert_eq!(fs::read_link(root.join("etc")).unwrap(), Path::new("/system/etc"), "a link in the file's place");
        assert!(meta("bin").is_dir(), "a folder in the link's place");
        assert_eq!(meta("bin").permissions().mode() & 0o7777, 0o751);
        assert_eq!(fs::read(root.join("system/bin/init")).unwrap(), b"new program");
        assert_eq!(meta("system/bin/init").permissions().mode() & 0o7777, 0o750);
        assert!(meta("system/bin/pipe").file_type().is_fifo());
        for e in &rd.entries {
            let m = meta(&e.name);
            assert_eq!(m.mtime(), 1_500_000_000, "{} has the archive's time", e.name);
            assert_eq!((m.uid(), m.gid()), (e.uid, e.gid), "{} has the archive's owner", e.name);
        }
    }

    #[test]
    fn a_folder_with_files_in_it_keeps_its_place() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("lib")).unwrap();
        fs::write(dir.path().join("lib/modules.load"), b"").unwrap();
        let rd = Ramdisk { entries: vec![mine("lib", S_IFREG | 0o644, b"a file")] };
        assert!(rd.unpack(dir.path()).is_err());
        assert!(dir.path().join("lib/modules.load").exists());
    }

    #[test]
    fn a_link_already_there_is_pointed_anew_keeping_the_rest_of_its_entry() {
        let mut rd = Ramdisk { entries: Vec::new() };
        rd.add("system/bin/ziptool", 0o755, b"binary".to_vec()).unwrap();
        rd.entries.push(Entry {
            name: "system/bin/unzip".into(),
            mode: S_IFLNK | 0o755,
            uid: 0,
            gid: 2000,
            mtime: 0,
            nlink: 1,
            dev: (0, 0),
            rdev: (0, 0),
            data: b"ziptool".to_vec(),
        });
        let count = rd.entries.len();
        rd.link("system/bin/unzip", "recovery").unwrap();
        let back = Ramdisk::decode(&rd.encode().unwrap()).unwrap();
        let unzip = back.find("system/bin/unzip").unwrap();
        assert_eq!((unzip.data.as_slice(), unzip.mode, unzip.gid), (b"recovery".as_slice(), S_IFLNK | 0o755, 2000));
        assert_eq!(back.entries.len(), count, "no second entry of the name");
        assert!(back.find("system/bin/ziptool").is_some(), "what it pointed to stays");
    }
}
