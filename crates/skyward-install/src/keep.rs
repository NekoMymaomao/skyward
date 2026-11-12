//! Keeping this recovery in the recovery partitions an install writes
//! over: this recovery as its partition holds it, what each slot's
//! partition held before the install, and which slots get it back after.
//!
//! A VAB phone whose recovery lives in `boot` keeps more than `boot`: an
//! update rewrites `boot`, `vendor_boot` and `dtbo` together, so the
//! recovery's kernel, its first-stage ramdisk and its device tree stay in
//! step. What is held from each slot is replayed into every slot the
//! install wrote over, so the whole boot set is kept as one.

use crate::image;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use skyward_vab::Slot;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// What a slot's partition holds: a digest of all of it, and whether that
/// is Skyward, any build of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held {
    pub digest: [u8; 32],
    pub skyward: bool,
}

/// One partition of the boot set this recovery keeps, held from the current
/// slot before an install.
struct KeptPartition {
    /// The partition's name, as in `/dev/block/by-name`.
    name: String,
    /// What the current slot's partition holds, to be written back.
    image: Vec<u8>,
    /// The digest treated as "this held image": the recovery's, for the
    /// partition it lives in, or the companion's own, held at boot.
    own: [u8; 32],
    /// Only the partition the recovery lives in can be Skyward.
    skyward: bool,
    /// What each slot held before the install, slot A first.
    before: [Held; 2],
}

/// This recovery, held from the recovery partitions among the links in
/// `dir` before an install; none when the partition of the `current` slot
/// does not hold Skyward, as when it was started without being flashed.
pub struct Kept {
    dir: PathBuf,
    /// `recovery_partition` first, then its companions, each held whole.
    partitions: Vec<KeptPartition>,
}

impl Kept {
    /// Hold this recovery from `partition` and its `companions` among the
    /// links in `dir`, before an install; none when `partition` of the
    /// `current` slot does not hold Skyward.
    pub fn hold(dir: &Path, current: Slot, partition: &str, companions: &[String]) -> Result<Option<Kept>> {
        let mut partitions = Vec::with_capacity(1 + companions.len());
        for (index, name) in std::iter::once(partition).chain(companions.iter().map(String::as_str)).enumerate() {
            let (a, image_a) = read(&image::device(dir, name, Slot::A))?;
            let (b, image_b) = read(&image::device(dir, name, Slot::B))?;
            let (held, image) = match current {
                Slot::A => (a, image_a),
                Slot::B => (b, image_b),
            };
            let skyward = index == 0 && held.skyward;
            partitions.push(KeptPartition { name: name.to_string(), image, own: held.digest, skyward, before: [a, b] });
        }
        if !partitions[0].skyward {
            return Ok(None);
        }
        Ok(Some(Kept { dir: dir.to_path_buf(), partitions }))
    }

    /// The slots whose boot set gets this recovery back, by what each
    /// partition holds once the install is done; `next` is the slot that
    /// starts next.
    pub fn wanted(&self, next: Slot) -> Result<Vec<Slot>> {
        let mut slots = Vec::new();
        for kept in &self.partitions {
            let (a, _) = read(&image::device(&self.dir, &kept.name, Slot::A))?;
            let (b, _) = read(&image::device(&self.dir, &kept.name, Slot::B))?;
            for slot in restore(kept.before, [a, b], kept.own, next) {
                if !slots.contains(&slot) {
                    slots.push(slot);
                }
            }
        }
        slots.sort_by_key(|slot| slot.number());
        slots.dedup();
        Ok(slots)
    }

    /// Write this recovery's boot set into `slots` and read each back;
    /// `progress` hears the bytes so far, of all.
    pub fn write(&self, slots: &[Slot], progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        for kept in &self.partitions {
            let devices: Vec<PathBuf> = slots.iter().map(|&slot| image::device(&self.dir, &kept.name, slot)).collect();
            image::flash(&mut Cursor::new(&kept.image), kept.image.len() as u64, &devices, progress)?;
        }
        Ok(())
    }
}

/// What the partition at `path` holds, read past any pages cached of it,
/// and all of it.
fn read(path: &Path) -> Result<(Held, Vec<u8>)> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    image::drop_cache(&file).with_context(|| format!("drop the cached pages of {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).with_context(|| format!("read {}", path.display()))?;
    let skyward = image::is_skyward(&mut Cursor::new(&bytes)).with_context(|| format!("look into {}", path.display()))?;
    Ok((Held { digest: Sha256::digest(&bytes).into(), skyward }, bytes))
}

/// The slots whose partition gets this recovery back once an install is
/// done, by what each held `before` and `after` it, slot A first: the slot
/// that starts next, `next`, unless it holds this build, whose digest is
/// `own`; and any other the install wrote over with something that is not
/// Skyward.
pub fn restore(before: [Held; 2], after: [Held; 2], own: [u8; 32], next: Slot) -> Vec<Slot> {
    [Slot::A, Slot::B]
        .into_iter()
        .filter(|&slot| {
            let k = slot.number() as usize;
            match slot == next {
                true => after[k].digest != own,
                false => after[k].digest != before[k].digest && !after[k].skyward,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::samples::recovery;

    const SKYWARD: Held = Held { digest: [1; 32], skyward: true };
    const OWN: [u8; 32] = SKYWARD.digest;
    /// Another build of Skyward.
    const OTHER_BUILD: Held = Held { digest: [4; 32], skyward: true };
    const STOCK: Held = Held { digest: [2; 32], skyward: false };
    const OLD: Held = Held { digest: [3; 32], skyward: false };

    #[test]
    fn the_slot_that_starts_next_gets_this_build() {
        assert_eq!(restore([SKYWARD, OLD], [SKYWARD, STOCK], OWN, Slot::B), [Slot::B], "an update or a package that went to slot B");
        assert_eq!(restore([SKYWARD, OLD], [SKYWARD, OLD], OWN, Slot::B), [Slot::B], "an older recovery there, left alone by the install");
        assert_eq!(restore([SKYWARD, OTHER_BUILD], [SKYWARD, OTHER_BUILD], OWN, Slot::B), [Slot::B], "another build of Skyward there");
        assert!(restore([SKYWARD, SKYWARD], [SKYWARD, SKYWARD], OWN, Slot::A).is_empty(), "this recovery is there already");
    }

    #[test]
    fn elsewhere_it_goes_back_only_where_the_install_wrote_something_else() {
        assert!(restore([SKYWARD, OLD], [SKYWARD, OLD], OWN, Slot::A).is_empty(), "a package that left recovery alone, such as a root installer");
        assert_eq!(restore([SKYWARD, OLD], [SKYWARD, STOCK], OWN, Slot::A), [Slot::B]);
        assert_eq!(restore([SKYWARD, OLD], [STOCK, STOCK], OWN, Slot::A), [Slot::A, Slot::B]);
        assert!(restore([SKYWARD, OLD], [SKYWARD, OTHER_BUILD], OWN, Slot::A).is_empty(), "a package that put another build of Skyward in");
    }

    /// Recovery partitions of `size` bytes holding `a` and `b`.
    fn partitions(a: &[u8], b: &[u8], size: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, image) in [("recovery_a", a), ("recovery_b", b)] {
            let mut bytes = image.to_vec();
            bytes.resize(size, 0);
            std::fs::write(dir.path().join(name), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn this_recovery_goes_back_where_an_install_wrote_over_it() {
        let (skyward, stock) = (recovery(true), recovery(false));
        let size = skyward.len().max(stock.len()) + 4096;
        let dir = partitions(&skyward, &stock, size);
        let kept = Kept::hold(dir.path(), Slot::A, "recovery", &[]).unwrap().expect("slot A's partition holds Skyward");
        assert!(kept.wanted(Slot::A).unwrap().is_empty(), "nothing written over");
        // A package puts the stock recovery in both slots.
        let mut written = stock.clone();
        written.resize(size, 0);
        std::fs::write(dir.path().join("recovery_a"), &written).unwrap();
        assert_eq!(kept.wanted(Slot::A).unwrap(), [Slot::A], "slot B held the stock recovery already");
        let mut told = Vec::new();
        kept.write(&[Slot::A], &mut |done, total| told.push((done, total))).unwrap();
        assert_eq!(told.last(), Some(&(2 * size as u64, 2 * size as u64)), "written and read back");
        let mut whole = skyward.clone();
        whole.resize(size, 0);
        assert_eq!(std::fs::read(dir.path().join("recovery_a")).unwrap(), whole);
        assert!(kept.wanted(Slot::A).unwrap().is_empty());
        assert_eq!(kept.wanted(Slot::B).unwrap(), [Slot::B], "an install that has slot B start next");
    }

    /// A boot set of `size` bytes in `boot`, `vendor_boot` and `dtbo`, each
    /// partition holding `boot_a`/`vendor_a`/`dtbo_a` and their B sides.
    fn boot_set(boot_a: &[u8], boot_b: &[u8], companion_a: &[u8], companion_b: &[u8], size: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, a, b) in [("boot", boot_a, boot_b), ("vendor_boot", companion_a, companion_b), ("dtbo", companion_a, companion_b)] {
            for (slot, image) in [(Slot::A, a), (Slot::B, b)] {
                let mut bytes = image.to_vec();
                bytes.resize(size, 0);
                std::fs::write(dir.path().join(format!("{name}{}", slot.suffix())), bytes).unwrap();
            }
        }
        dir
    }

    #[test]
    fn companions_go_back_with_the_recovery_they_were_flashed_with() {
        let (skyward, stock) = (recovery(true), recovery(false));
        let size = skyward.len().max(stock.len()) + 4096;
        let companions = ["vendor_boot".to_string(), "dtbo".to_string()];
        // Skyward flashed into boot, with matching vendor_boot and dtbo.
        let dir = boot_set(&skyward, &stock, &stock, &stock, size);
        let kept = Kept::hold(dir.path(), Slot::A, "boot", &companions).unwrap().expect("boot holds Skyward");
        // An install rewrites slot B's whole boot set to stock.
        for name in ["boot", "vendor_boot", "dtbo"] {
            let mut written = stock.clone();
            written.resize(size, 0);
            std::fs::write(dir.path().join(format!("{name}_b")), &written).unwrap();
        }
        assert_eq!(kept.wanted(Slot::B).unwrap(), [Slot::B], "slot B's boot set was rewritten");
        kept.write(&[Slot::B], &mut |_, _| {}).unwrap();
        let mut whole = skyward.clone();
        whole.resize(size, 0);
        assert_eq!(std::fs::read(dir.path().join("boot_b")).unwrap(), whole, "Skyward written back to boot");
        let mut companion = stock.clone();
        companion.resize(size, 0);
        assert_eq!(std::fs::read(dir.path().join("vendor_boot_b")).unwrap(), companion, "the companion kept with it");
        assert_eq!(std::fs::read(dir.path().join("dtbo_b")).unwrap(), companion);
        assert!(kept.wanted(Slot::B).unwrap().is_empty(), "nothing left to write");
    }

    #[test]
    fn nothing_is_kept_when_this_recovery_was_started_without_being_flashed() {
        let dir = partitions(&recovery(false), &recovery(false), 1 << 20);
        assert!(Kept::hold(dir.path(), Slot::B, "recovery", &[]).unwrap().is_none());
        let erased = partitions(&recovery(true), &[], 1 << 20);
        assert!(Kept::hold(erased.path(), Slot::B, "recovery", &[]).unwrap().is_none(), "an erased partition");
        assert!(Kept::hold(erased.path(), Slot::A, "recovery", &[]).unwrap().is_some());
    }
}
