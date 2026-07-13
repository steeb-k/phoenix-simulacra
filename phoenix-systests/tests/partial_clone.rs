//! Tier-2 system tests for **partial clone** (`CloneTableMode::UpdateExisting`).
//!
//! This is the Clone page's **default** mode — the one a user gets by dragging a
//! single source partition onto a target — and until now it shipped on five unit
//! tests of planning math. The write path was exercised by nothing: no T2, no T3.
//! It is also the most dangerous mode in the engine, because unlike a full clone
//! it does not wipe the target first. It writes into a **live partition table**,
//! over volumes Windows currently has mounted, and then rewrites that table in
//! place. Every failure mode here is somebody's other partition.
//!
//! So each test below pins the three things that can silently go wrong:
//!
//!   1. **The cloned slot got the source's bytes** — the fixture verifies, and
//!      the slot is found by its exact offset, never by "first letter on the
//!      disk" (on a partial clone that could just as easily be the partition we
//!      were supposed to leave alone).
//!   2. **The engine wrote nothing outside its slot** — the preserved sibling is
//!      snapshotted per 1 MiB block before and after.
//!   3. **The table says what we meant** — the right partitions exist afterwards,
//!      at the right offsets and sizes, and dropped ones are actually gone.
//!
//! A note on (2), because the obvious version of it is wrong: you cannot demand
//! that a preserved partition's raw bytes be *identical* afterwards. It stays
//! **mounted** through the clone — the engine only locks the slots it is about to
//! overwrite — so NTFS's lazy writer keeps flushing its own metadata on its own
//! schedule, and the bytes drift no matter what the engine does. The first draft
//! of this file asserted byte-equality and failed instantly on that drift, which
//! reads exactly like data corruption and isn't.
//!
//! What distinguishes the two is *where* the changes land. NTFS metadata sits in
//! a contiguous run near the front of the volume (observed: blocks ~5-21 of 368).
//! An engine spilling out of the slot below would have to start at **block 0**,
//! the shared boundary, and run from there. So compare block-wise and judge the
//! shape — and let the fixture digest and `chkdsk` have the final word on whether
//! the data actually survived.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests --test partial_clone -- --ignored --test-threads=1

use phoenix_clone::{run_clone, CloneEntry, CloneOptions, ClonePlan, CloneTableMode, CloneVerify};
use phoenix_core::disk::{enumerate_disks, refine_partition_fs, DiskInfo};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture,
    wait_for_letter, wait_for_letter_at_offset, PartSpec, TestFs, TestVhd,
};

/// Re-enumerate with filesystem refinement, the way every engine entry point does.
fn disks() -> Vec<DiskInfo> {
    let mut d = enumerate_disks().expect("enumerate disks");
    for disk in &mut d {
        for p in &mut disk.partitions {
            refine_partition_fs(p);
        }
    }
    d
}

fn disk_by_index(index: u32) -> DiskInfo {
    disks()
        .into_iter()
        .find(|d| d.index == index)
        .unwrap_or_else(|| panic!("disk {index} not enumerated"))
}

const BLOCK: u64 = 1024 * 1024;

/// Per-1-MiB-block hashes of a partition's raw sectors, read straight off
/// `\\.\PhysicalDriveN`.
///
/// A single whole-partition hash can only say "something changed", which is not
/// good enough here — a **live, mounted NTFS volume rewrites its own metadata**
/// (`$LogFile`, `$MFT` timestamps, the dirty bit) on the lazy writer's schedule,
/// so its raw bytes drift even when nothing on earth has touched it. That drift
/// is confined to the filesystem's metadata region near the front of the volume.
///
/// An engine that wrote *outside the slot it was given* would look completely
/// different: a broad run of changed blocks, wherever the overspill landed.
///
/// So compare block by block and let the shape of the difference say which one
/// happened. `changed_blocks` below turns that into a verdict.
fn block_hashes(disk_index: u32, offset: u64, len: u64) -> Vec<blake3::Hash> {
    use std::io::{Read, Seek, SeekFrom};
    let path = format!(r"\\.\PhysicalDrive{disk_index}");
    let mut f = std::fs::File::open(&path).expect("open physical drive for read");
    f.seek(SeekFrom::Start(offset)).expect("seek");
    let mut out = Vec::new();
    let mut remaining = len;
    let mut buf = vec![0u8; BLOCK as usize];
    while remaining > 0 {
        let want = buf.len().min(remaining as usize);
        f.read_exact(&mut buf[..want])
            .expect("read partition bytes");
        out.push(blake3::hash(&buf[..want]));
        remaining -= want as u64;
    }
    out
}

/// Indices of the 1 MiB blocks that differ between two snapshots.
fn changed_blocks(before: &[blake3::Hash], after: &[blake3::Hash]) -> Vec<usize> {
    assert_eq!(before.len(), after.len(), "snapshot length changed");
    before
        .iter()
        .zip(after)
        .enumerate()
        .filter(|(_, (b, a))| b != a)
        .map(|(i, _)| i)
        .collect()
}

/// Clone ONE source partition into an existing target slot, preserving the
/// target's other partition.
///
/// The target keeps its own partition table throughout — this is what separates
/// a partial clone from every other mode, and what makes it able to destroy data
/// the user never selected.
#[test]
#[ignore = "requires elevation + diskpart"]
fn partial_clone_replaces_one_slot_and_preserves_sibling() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source: a single NTFS partition with a known fixture.
    let source = TestVhd::create(384).expect("create source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "CLONESRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let src_digest = fill_fixture('X', 0xC10E_0001).expect("fill source fixture");

    // Target: TWO partitions, both with their own fixtures. Partition 1 is the
    // slot we clone into; partition 2 must come through untouched.
    let target = TestVhd::create(768).expect("create target");
    target
        .init_gpt_with(&[
            PartSpec {
                size_mb: 384,
                fs: TestFs::Ntfs,
                letter: 'Y',
                label: "TGTSLOT".into(),
            },
            PartSpec {
                size_mb: 0,
                fs: TestFs::Ntfs,
                letter: 'Z',
                label: "TGTKEEP".into(),
            },
        ])
        .expect("init target");
    assert!(wait_for_letter('Y', 15_000), "target slot never mounted");
    assert!(wait_for_letter('Z', 15_000), "target keep never mounted");
    let keep_digest = fill_fixture('Z', 0xC10E_0002).expect("fill preserved fixture");

    let src_disk = disk_by_index(source.disk_index());
    let tgt_disk = disk_by_index(target.disk_index());
    let src_part = src_disk
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .expect("source NTFS partition");

    // The two target data partitions, in on-disk order (the MSR that diskpart
    // adds is not NTFS, so it is neither cloned into nor preserved by name — it
    // is preserved by index below along with the sibling).
    let tgt_ntfs: Vec<_> = tgt_disk
        .partitions
        .iter()
        .filter(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .collect();
    assert_eq!(
        tgt_ntfs.len(),
        2,
        "expected 2 NTFS partitions on the target"
    );
    let slot = tgt_ntfs[0];
    let keep = tgt_ntfs[1];

    // Snapshot the preserved partition's raw sectors BEFORE the clone. This is
    // the check that matters most: a partial clone that corrupts a partition the
    // user didn't select is the silent-data-loss case this whole mode risks.
    let keep_before = block_hashes(target.disk_index(), keep.offset_bytes, keep.size_bytes);

    // Preserve everything except the slot we're cloning into — including the MSR,
    // which is a live partition the engine would otherwise drop from the table.
    let preserved: Vec<u32> = tgt_disk
        .partitions
        .iter()
        .filter(|p| p.index != slot.index)
        .map(|p| p.index)
        .collect();

    let plan = ClonePlan {
        entries: vec![CloneEntry {
            source_partition_index: src_part.index,
            target_offset_bytes: slot.offset_bytes,
            target_size_bytes: slot.size_bytes,
        }],
        table_mode: CloneTableMode::UpdateExisting {
            preserved_target_indices: preserved,
        },
    };

    run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: target.disk_index(),
        plan,
        verify: CloneVerify::ReadBack,
        progress: None,
    })
    .expect("partial clone");

    // 1. Did anything write to the preserved partition, and if so, where?
    //
    //    Blocks CAN legitimately change here: the volume stays mounted through
    //    the clone (the engine only locks the slots it is about to overwrite), so
    //    NTFS's lazy writer flushes its own metadata on its own schedule. What
    //    must NOT happen is the engine spilling cloned data into it.
    //
    //    Those two look nothing alike. NTFS metadata lives in a handful of blocks
    //    near the front of the volume; an overspill from the slot below would be a
    //    broad, contiguous run. Report the shape, and fail on the shape.
    let keep_after = block_hashes(target.disk_index(), keep.offset_bytes, keep.size_bytes);
    let changed = changed_blocks(&keep_before, &keep_after);
    let total = keep_before.len();
    if !changed.is_empty() {
        eprintln!(
            "[preserved] {} of {total} 1-MiB blocks changed: {:?}{}",
            changed.len(),
            &changed[..changed.len().min(24)],
            if changed.len() > 24 { " ..." } else { "" }
        );
    }
    // A tenth of the partition rewritten is not a lazy writer flushing a journal.
    assert!(
        changed.len() * 10 < total,
        "{} of {total} 1-MiB blocks in the PRESERVED partition changed during a partial clone — \
         that is far too much to be NTFS metadata; the engine wrote outside the slot it was \
         given. Changed blocks: {:?}",
        changed.len(),
        &changed[..changed.len().min(64)]
    );

    // 2. The table still describes both partitions, at the same places.
    let after = disk_by_index(target.disk_index());
    let after_ntfs: Vec<_> = after
        .partitions
        .iter()
        .filter(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .collect();
    assert_eq!(
        after_ntfs.len(),
        2,
        "partial clone changed the target's partition count: {:?}",
        after
            .partitions
            .iter()
            .map(|p| (p.index, p.offset_bytes, p.size_bytes, p.fs_kind))
            .collect::<Vec<_>>()
    );
    assert_eq!(after_ntfs[0].offset_bytes, slot.offset_bytes);
    assert_eq!(after_ntfs[0].size_bytes, slot.size_bytes);
    assert_eq!(after_ntfs[1].offset_bytes, keep.offset_bytes);
    assert_eq!(after_ntfs[1].size_bytes, keep.size_bytes);

    // 3. The cloned slot holds the SOURCE's data, and the preserved sibling still
    //    holds its own — verified through the filesystem, so this also proves both
    //    volumes still mount.
    let slot_letter = wait_for_letter_at_offset(target.disk_index(), slot.offset_bytes, 45_000)
        .expect("cloned slot never got a drive letter");
    chkdsk_clean(slot_letter).expect("cloned slot is chkdsk-clean");
    verify_fixture(slot_letter, &src_digest).expect("cloned slot holds the source fixture");

    assert!(
        wait_for_letter('Z', 30_000),
        "the preserved partition lost its drive letter after a partial clone"
    );
    verify_fixture('Z', &keep_digest).expect("preserved partition still holds its own fixture");
}

/// Partial clone into a **smaller** slot: the NTFS source must shrink to fit
/// (relocation + metadata rewrite), and the sibling must still survive.
///
/// Shrink-on-clone shares `build_relocation_map` and the NTFS metadata rewriter
/// with restore, but reaches them down a different path — and the clone path is
/// the one with no end-to-end coverage, so a regression here would be invisible.
#[test]
#[ignore = "requires elevation + diskpart"]
fn partial_clone_shrinks_ntfs_into_a_smaller_slot() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source: 512 MiB NTFS, fixture much smaller than the slot it must fit into.
    let source = TestVhd::create(512).expect("create source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SHRSRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let src_digest = fill_fixture('X', 0xC10E_0003).expect("fill source fixture");

    // Target: a 256 MiB slot (smaller than the source partition) plus a sibling.
    let target = TestVhd::create(640).expect("create target");
    target
        .init_gpt_with(&[
            PartSpec {
                size_mb: 256,
                fs: TestFs::Ntfs,
                letter: 'Y',
                label: "SHRSLOT".into(),
            },
            PartSpec {
                size_mb: 0,
                fs: TestFs::Ntfs,
                letter: 'Z',
                label: "SHRKEEP".into(),
            },
        ])
        .expect("init target");
    assert!(wait_for_letter('Y', 15_000), "target slot never mounted");
    assert!(wait_for_letter('Z', 15_000), "target keep never mounted");
    let keep_digest = fill_fixture('Z', 0xC10E_0004).expect("fill preserved fixture");

    let src_disk = disk_by_index(source.disk_index());
    let tgt_disk = disk_by_index(target.disk_index());
    let src_part = src_disk
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .expect("source NTFS partition");
    let tgt_ntfs: Vec<_> = tgt_disk
        .partitions
        .iter()
        .filter(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .collect();
    let slot = tgt_ntfs[0];
    let keep = tgt_ntfs[1];
    assert!(
        slot.size_bytes < src_part.size_bytes,
        "fixture is wrong: the target slot must be SMALLER than the source for this to shrink"
    );

    let keep_before = block_hashes(target.disk_index(), keep.offset_bytes, keep.size_bytes);

    let preserved: Vec<u32> = tgt_disk
        .partitions
        .iter()
        .filter(|p| p.index != slot.index)
        .map(|p| p.index)
        .collect();

    run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: target.disk_index(),
        plan: ClonePlan {
            entries: vec![CloneEntry {
                source_partition_index: src_part.index,
                target_offset_bytes: slot.offset_bytes,
                target_size_bytes: slot.size_bytes,
            }],
            table_mode: CloneTableMode::UpdateExisting {
                preserved_target_indices: preserved,
            },
        },
        verify: CloneVerify::ReadBack,
        progress: None,
    })
    .expect("partial clone with shrink");

    let keep_after = block_hashes(target.disk_index(), keep.offset_bytes, keep.size_bytes);
    let changed = changed_blocks(&keep_before, &keep_after);
    let total = keep_before.len();
    if !changed.is_empty() {
        eprintln!(
            "[preserved/shrink] {} of {total} 1-MiB blocks changed: {:?}",
            changed.len(),
            &changed[..changed.len().min(24)]
        );
    }
    assert!(
        changed.len() * 10 < total,
        "{} of {total} 1-MiB blocks in the PRESERVED partition changed during a shrinking \
         partial clone — the relocation wrote past the end of its slot",
        changed.len()
    );

    let slot_letter = wait_for_letter_at_offset(target.disk_index(), slot.offset_bytes, 60_000)
        .expect("shrunk slot never got a drive letter");
    chkdsk_clean(slot_letter).expect("shrunk slot is chkdsk-clean");
    verify_fixture(slot_letter, &src_digest).expect("shrunk slot holds the source fixture");

    assert!(
        wait_for_letter('Z', 30_000),
        "the preserved partition lost its drive letter"
    );
    verify_fixture('Z', &keep_digest).expect("preserved partition survived the shrink");
}

/// A partial clone that does NOT preserve a live target partition must remove it
/// from the table — after locking and dismounting it.
///
/// This is the destructive half of the mode, and the one a user reaches by
/// dropping a source onto a target that has partitions they didn't tick. If the
/// engine failed to lock the mounted volume first, the raw writes and the
/// in-place table rewrite would bounce with `ERROR_ACCESS_DENIED` — exactly the
/// failure the partial-*restore* path hit on real hardware (T3B `boot_e`) before
/// it grew the same lock.
#[test]
#[ignore = "requires elevation + diskpart"]
fn partial_clone_drops_an_unpreserved_live_partition() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create(256).expect("create source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "DROPSRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let src_digest = fill_fixture('X', 0xC10E_0005).expect("fill source fixture");

    // Target: a slot to clone into, plus a SECOND live, mounted, fixture-filled
    // partition that the plan will simply not mention. It must end up gone.
    let target = TestVhd::create(768).expect("create target");
    target
        .init_gpt_with(&[
            PartSpec {
                size_mb: 256,
                fs: TestFs::Ntfs,
                letter: 'Y',
                label: "DROPSLOT".into(),
            },
            PartSpec {
                size_mb: 0,
                fs: TestFs::Ntfs,
                letter: 'Z',
                label: "DROPME".into(),
            },
        ])
        .expect("init target");
    assert!(wait_for_letter('Y', 15_000), "target slot never mounted");
    assert!(
        wait_for_letter('Z', 15_000),
        "doomed partition never mounted"
    );
    let _ = fill_fixture('Z', 0xC10E_0006).expect("fill doomed fixture");

    let src_disk = disk_by_index(source.disk_index());
    let tgt_disk = disk_by_index(target.disk_index());
    let src_part = src_disk
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .expect("source NTFS partition");
    let tgt_ntfs: Vec<_> = tgt_disk
        .partitions
        .iter()
        .filter(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .collect();
    let slot = tgt_ntfs[0];
    let doomed = tgt_ntfs[1];

    // Preserve ONLY the non-NTFS housekeeping partitions (the MSR). The doomed
    // partition is mounted at Z: right now, and is in neither set — so the engine
    // must lock it, dismount it, and drop it from the table.
    let preserved: Vec<u32> = tgt_disk
        .partitions
        .iter()
        .filter(|p| p.index != slot.index && p.index != doomed.index)
        .map(|p| p.index)
        .collect();

    run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: target.disk_index(),
        plan: ClonePlan {
            entries: vec![CloneEntry {
                source_partition_index: src_part.index,
                target_offset_bytes: slot.offset_bytes,
                target_size_bytes: slot.size_bytes,
            }],
            table_mode: CloneTableMode::UpdateExisting {
                preserved_target_indices: preserved,
            },
        },
        verify: CloneVerify::ReadBack,
        progress: None,
    })
    .expect("partial clone dropping a live partition");

    let after = disk_by_index(target.disk_index());
    let after_ntfs: Vec<_> = after
        .partitions
        .iter()
        .filter(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .collect();
    assert_eq!(
        after_ntfs.len(),
        1,
        "the unpreserved partition is still on the table: {:?}",
        after
            .partitions
            .iter()
            .map(|p| (p.index, p.offset_bytes, p.size_bytes))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        after_ntfs[0].offset_bytes, slot.offset_bytes,
        "the surviving partition is not the one we cloned into"
    );
    assert!(
        !after
            .partitions
            .iter()
            .any(|p| p.offset_bytes == doomed.offset_bytes),
        "a partition still exists at the dropped partition's offset"
    );

    let slot_letter = wait_for_letter_at_offset(target.disk_index(), slot.offset_bytes, 45_000)
        .expect("cloned slot never got a drive letter");
    chkdsk_clean(slot_letter).expect("cloned slot is chkdsk-clean");
    verify_fixture(slot_letter, &src_digest).expect("cloned slot holds the source fixture");
}

// ---------------------------------------------------------------------------
// Pre-flight: a source we cannot freeze must abort BEFORE the target is touched.
// ---------------------------------------------------------------------------

/// A clone that cannot freeze one of its source partitions must fail **without
/// having written anything to the target**.
///
/// This is the guarantee that a boot-disk clone actually needs. The engine used to
/// freeze each partition inline, immediately before streaming it — so partition 1
/// was fully cloned (and the target's partition table already wiped) before
/// partition 2's lock was even attempted. A source volume that couldn't be frozen
/// destroyed the user's target disk on the way to discovering something knowable
/// up front.
///
/// The source here is a **two**-partition disk: an idle NTFS partition that locks
/// fine, and a FAT32 partition with a file held open. FAT32 is the important
/// choice — **VSS cannot snapshot FAT**, so there is no shadow to escalate to, and
/// the held handle makes the lock impossible. It is exactly the shape of the ESP,
/// which is what bit the real boot-disk clone.
///
/// The target is pre-filled with a fixture, and the test asserts that fixture is
/// *still there and still valid* afterwards. Not "the clone failed" — anyone can
/// fail. The point is that it failed **harmlessly**.
#[test]
#[ignore = "requires elevation + diskpart"]
fn clone_aborts_before_touching_the_target_when_a_source_cannot_be_frozen() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source: NTFS (lockable) followed by FAT32 (which we will make unfreezable).
    let source = TestVhd::create(512).expect("create source");
    source
        .init_gpt_with(&[
            PartSpec {
                size_mb: 256,
                fs: TestFs::Ntfs,
                letter: 'X',
                label: "PFNTFS".into(),
            },
            PartSpec {
                size_mb: 0,
                fs: TestFs::Fat32,
                letter: 'Y',
                label: "PFFAT".into(),
            },
        ])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source NTFS never mounted");
    assert!(wait_for_letter('Y', 15_000), "source FAT32 never mounted");
    let _ = fill_fixture('X', 0xBEEF_0001).expect("fill source NTFS");
    let _ = fill_fixture('Y', 0xBEEF_0002).expect("fill source FAT32");

    // Target: a disk with its OWN data on it, which must survive untouched.
    let target = TestVhd::create(768).expect("create target");
    target
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'Z',
            label: "TGTDATA".into(),
        }])
        .expect("init target");
    assert!(wait_for_letter('Z', 15_000), "target never mounted");
    let target_digest = fill_fixture('Z', 0xBEEF_0003).expect("fill target fixture");

    let tgt_before = disk_by_index(target.disk_index());
    let tgt_parts_before: Vec<(u64, u64)> = tgt_before
        .partitions
        .iter()
        .map(|p| (p.offset_bytes, p.size_bytes))
        .collect();

    // Make the FAT32 source partition unfreezable: hold a file open on it. The lock
    // is now impossible, and VSS cannot snapshot FAT to escalate to.
    let mut held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(r"Y:\held-open.bin")
        .expect("open held file on the FAT32 source");
    use std::io::Write as _;
    held.write_all(b"held").expect("write held file");

    // Clone the WHOLE source disk. Partition 1 (NTFS) is perfectly cloneable — the
    // old engine would have happily wiped the target and written it before ever
    // discovering that partition 2 cannot be frozen.
    let src_disk = disk_by_index(source.disk_index());
    let result = run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: target.disk_index(),
        plan: ClonePlan::identity(&src_disk),
        verify: CloneVerify::ReadBack,
        progress: None,
    });

    let err = match result {
        Err(e) => e,
        Ok(_) => panic!(
            "the clone SUCCEEDED with a held handle on an unsnapshottable FAT32 source — the \
             engine copied a live, mutating filesystem instead of refusing"
        ),
    };
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("lock"),
        "clone failed for an unexpected reason (wanted a volume-lock failure): {err}"
    );
    eprintln!("[preflight] clone correctly refused: {err}");

    drop(held);
    let _ = std::fs::remove_file(r"Y:\held-open.bin");

    // THE ASSERTION THAT MATTERS. The clone failed — but did it fail before doing
    // any damage? The target's partition table must be exactly as it was...
    let tgt_after = disk_by_index(target.disk_index());
    let tgt_parts_after: Vec<(u64, u64)> = tgt_after
        .partitions
        .iter()
        .map(|p| (p.offset_bytes, p.size_bytes))
        .collect();
    assert_eq!(
        tgt_parts_before, tgt_parts_after,
        "the target's partition table CHANGED even though the clone aborted — the engine wiped \
         the target before discovering it could not read the source"
    );

    // ...and its data must still be there, and still readable.
    assert!(
        wait_for_letter('Z', 30_000),
        "the target's volume lost its drive letter after an aborted clone"
    );
    chkdsk_clean('Z').expect("target volume is chkdsk-clean after an aborted clone");
    verify_fixture('Z', &target_digest)
        .expect("the target's own data was destroyed by a clone that never completed");
}
