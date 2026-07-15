//! Tier-3 (real hardware): the **disk-to-disk clone matrix**, between two real
//! physical disks.
//!
//! Everything in `clone.rs` and `partial_clone.rs` runs against VHDX fixtures,
//! which are aligned, unfragmented, and lie about nothing. Real media is where
//! the interesting bugs live — the `div_ceil` phantom-cluster silent-data-loss
//! bug only ever showed itself on real fragmented NTFS. This file runs the same
//! clone modes a user can reach from the Clone page, against two real disks.
//!
//! ## ⚠️ BOTH DISKS ARE WIPED
//!
//! Source and target are each opted into by env var and each pass the full
//! `RealDisk` safety gate — boot/system disks are refused outright, sizes are
//! bounded, and a non-removable disk additionally demands `PHOENIX_T3_ALLOW_FIXED`
//! plus an exact serial pin. The *source* is gated just as hard as the target,
//! because these tests lay their own fixture onto it before cloning it: it is
//! every bit as destroyable.
//!
//! Without `PHOENIX_T3_SRC_DISK` **and** `PHOENIX_T3_DISK`, every test here skips
//! cleanly and writes nothing.
//!
//! ## Running it
//!
//! The intended rig is a small USB flash stick (source, MBR — Windows will not
//! make removable media GPT) and a large external HDD (target, GPT):
//!
//! ```powershell
//! # SOURCE: 32 GB USB flash. Removable, so the basic gates apply.
//! $env:PHOENIX_T3_SRC_DISK   = "2"
//! $env:PHOENIX_T3_SRC_SERIAL = "<exact serial>"      # optional for removable
//!
//! # TARGET: 4 TB external HDD. Non-removable → the strict gates apply.
//! $env:PHOENIX_T3_DISK        = "3"
//! $env:PHOENIX_T3_ALLOW_FIXED = "1"
//! $env:PHOENIX_T3_SERIAL      = "<exact serial>"     # MANDATORY here
//!
//! # Size window must admit BOTH disks (32 GB and 4 TB).
//! $env:PHOENIX_T3_MIN_GB = "16"
//! $env:PHOENIX_T3_MAX_GB = "4200"
//!
//! cargo test -p phoenix-systests --test real_clone -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Run it **staged, one test at a time** — a long unbroken run of disk churn has
//! bugchecked the dev box before (see docs/TESTING.md). `scripts/run-real-clone.ps1`
//! does that for you.
//!
//! ## Why the partitions are small
//!
//! The fixtures are a few GB, not 4 TB. Clone streams *used blocks*, so a mostly
//! empty source copies in seconds — but `chkdsk` on a grown multi-TB NTFS volume
//! does not, and a test nobody will wait for is a test nobody runs. Growth is
//! therefore exercised into a bounded larger slot rather than into all 4 TB.

use phoenix_clone::{run_clone, CloneEntry, CloneOptions, ClonePlan, CloneTableMode, CloneVerify};
use phoenix_core::disk::{enumerate_disks, refine_partition_fs, DiskInfo, FilesystemKind};
use phoenix_systests::{
    chkdsk_clean, fill_fixture, require_admin, verify_fixture, wait_for_letter,
    wait_for_letter_at_offset, FixtureDigest, PartSpec, RealDisk, TestFs,
};

const MB: u64 = 1024 * 1024;

/// Source layout: NTFS + exFAT on MBR, which is what a real USB stick looks like
/// (Windows refuses to make removable media GPT, so MBR is not a choice here).
const SRC_NTFS_MB: u64 = 3072;
const SRC_EXFAT_MB: u64 = 2048;

/// Acquire both disks, or skip. Returns `None` when either opt-in is absent, so
/// a developer without the rig attached runs `cargo test` and nothing happens.
fn rig() -> Option<(RealDisk, RealDisk)> {
    require_admin();
    let src = RealDisk::acquire_source().expect("source disk failed its safety gate");
    let tgt = RealDisk::acquire().expect("target disk failed its safety gate");
    match (src, tgt) {
        (Some(s), Some(t)) => {
            assert_ne!(
                s.index(),
                t.index(),
                "source and target are the SAME disk — that would clone a disk over itself"
            );
            eprintln!(
                "[T3-clone] source = disk {} ({}), target = disk {} ({})",
                s.index(),
                if s.is_removable() {
                    "removable"
                } else {
                    "fixed"
                },
                t.index(),
                if t.is_removable() {
                    "removable"
                } else {
                    "fixed"
                },
            );
            Some((s, t))
        }
        _ => {
            eprintln!(
                "[T3-clone] SKIPPED — set PHOENIX_T3_SRC_DISK and PHOENIX_T3_DISK to run \
                 (nothing was written)"
            );
            None
        }
    }
}

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

/// Wipe the source and lay down a known MBR NTFS + exFAT fixture.
///
/// Rebuilt for every test rather than shared: a clone test that inherited the
/// previous test's half-modified source would be reporting on a disk state
/// nobody chose, and the failure would be unreproducible.
fn build_source(src: &RealDisk) -> (FixtureDigest, FixtureDigest) {
    src.clean().expect("clean source");
    src.layout(
        false, // MBR — the source is removable, and Windows won't make that GPT
        &[
            PartSpec {
                size_mb: SRC_NTFS_MB,
                fs: TestFs::Ntfs,
                letter: 'R',
                label: "SRCNTFS".into(),
            },
            PartSpec {
                size_mb: SRC_EXFAT_MB,
                fs: TestFs::Exfat,
                letter: 'S',
                label: "SRCEXFAT".into(),
            },
        ],
    )
    .expect("lay out source");
    assert!(wait_for_letter('R', 30_000), "source NTFS never mounted");
    assert!(wait_for_letter('S', 30_000), "source exFAT never mounted");
    let ntfs = fill_fixture('R', 0x7301).expect("fill source NTFS fixture");
    let exfat = fill_fixture('S', 0x7302).expect("fill source exFAT fixture");
    eprintln!("[T3-clone] source built: NTFS + exFAT on MBR");
    (ntfs, exfat)
}

/// The source's data partitions, in on-disk order.
fn source_parts(src: &RealDisk) -> Vec<phoenix_core::disk::PartitionInfo> {
    let d = disk_by_index(src.index());
    let mut parts: Vec<_> = d
        .partitions
        .into_iter()
        .filter(|p| matches!(p.fs_kind, FilesystemKind::Ntfs | FilesystemKind::Exfat))
        .collect();
    parts.sort_by_key(|p| p.offset_bytes);
    assert_eq!(parts.len(), 2, "source should have NTFS + exFAT");
    parts
}

fn run(plan: ClonePlan, src: &RealDisk, tgt: &RealDisk, what: &str) {
    eprintln!("[T3-clone] {what}: {} entries", plan.entries.len());
    for e in &plan.entries {
        eprintln!(
            "[T3-clone]   src part {} -> off {} size {} ({:.1} GB)",
            e.source_partition_index,
            e.target_offset_bytes,
            e.target_size_bytes,
            e.target_size_bytes as f64 / 1e9
        );
    }
    let started = std::time::Instant::now();
    run_clone(CloneOptions {
        source_disk_index: src.index(),
        target_disk_index: tgt.index(),
        plan,
        // ReadBack: re-read the target's sectors and compare. On real media this
        // is the check that matters — it is what caught the phantom-cluster bug.
        verify: CloneVerify::ReadBack,
        convert_sector_size: false,
        progress: None,
    })
    .unwrap_or_else(|e| panic!("{what} failed: {e}"));
    eprintln!("[T3-clone] {what} OK in {:?}", started.elapsed());
}

/// Verify a cloned partition by finding it at its exact target offset — never by
/// "first letter on the disk", which on a partial clone can just as easily be the
/// partition we were told to leave alone.
fn verify_at(tgt: &RealDisk, offset: u64, digest: &FixtureDigest, chkdsk: bool, what: &str) {
    let letter = wait_for_letter_at_offset(tgt.index(), offset, 120_000).unwrap_or_else(|| {
        panic!("{what}: cloned partition at offset {offset} never got a letter")
    });
    eprintln!("[T3-clone] {what} mounted at {letter}:");
    if chkdsk {
        chkdsk_clean(letter).unwrap_or_else(|e| panic!("{what}: chkdsk not clean: {e:#}"));
    }
    verify_fixture(letter, digest).unwrap_or_else(|e| panic!("{what}: fixture mismatch: {e:#}"));
}

// ---------------------------------------------------------------------------
// 1. Full clone, re-initializing the target as GPT (MBR source -> GPT target).
// ---------------------------------------------------------------------------

/// The headline case: clone a whole MBR USB stick onto a big GPT disk, switching
/// the partition table style on the way.
///
/// This is what the Clone page's style switch does, and it is the only mode that
/// can get an MBR source onto a >2 TiB disk usefully — a `ReinitMatchSource`
/// clone would leave the target MBR, which cannot address past 2 TiB (see the
/// next test, which pins exactly that).
#[test]
#[ignore = "requires elevation + two real disks (PHOENIX_T3_SRC_DISK + PHOENIX_T3_DISK)"]
fn real_clone_full_mbr_source_to_gpt_target() {
    let Some((src, tgt)) = rig() else { return };
    let (ntfs_digest, exfat_digest) = build_source(&src);
    tgt.clean().expect("clean target");

    let parts = source_parts(&src);
    // Same sizes, packed after a 1 MiB GPT-safe lead-in.
    let mut off = MB;
    let entries: Vec<CloneEntry> = parts
        .iter()
        .map(|p| {
            let e = CloneEntry {
                source_partition_index: p.index,
                target_offset_bytes: off,
                target_size_bytes: p.size_bytes,
            };
            off += p.size_bytes;
            e
        })
        .collect();
    let ntfs_off = entries[0].target_offset_bytes;
    let exfat_off = entries[1].target_offset_bytes;

    run(
        ClonePlan {
            entries,
            table_mode: CloneTableMode::ReinitAs { gpt: true },
        },
        &src,
        &tgt,
        "full clone MBR->GPT",
    );

    let after = disk_by_index(tgt.index());
    assert!(
        after.is_gpt,
        "target should have been re-initialized as GPT"
    );

    verify_at(&tgt, ntfs_off, &ntfs_digest, true, "NTFS");
    // exFAT: chkdsk is skipped — the fixture digest is the real check, and chkdsk
    // on exFAT adds minutes without adding a claim we aren't already making.
    verify_at(&tgt, exfat_off, &exfat_digest, false, "exFAT");
}

// ---------------------------------------------------------------------------
// 2. Full clone matching the source's style (MBR target on a 4 TB disk).
// ---------------------------------------------------------------------------

/// `ReinitMatchSource` from an MBR source leaves the target **MBR** — which on a
/// 4 TB disk means the partition table can only address the first 2 TiB.
///
/// The engine clamps the layout to `MBR_ADDRESSABLE_BYTES` for exactly this. The
/// failure it prevents is nasty and quiet: `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` takes
/// 64-bit values and the data writes succeed, so nothing errors — you just get a
/// partition Windows can't mount, because the on-disk table can't represent where
/// it put it. Worth pinning on real hardware big enough to actually cross the line.
#[test]
#[ignore = "requires elevation + two real disks"]
fn real_clone_full_match_source_keeps_mbr() {
    let Some((src, tgt)) = rig() else { return };
    let (ntfs_digest, exfat_digest) = build_source(&src);
    tgt.clean().expect("clean target");

    let parts = source_parts(&src);
    let mut off = MB;
    let entries: Vec<CloneEntry> = parts
        .iter()
        .map(|p| {
            let e = CloneEntry {
                source_partition_index: p.index,
                target_offset_bytes: off,
                target_size_bytes: p.size_bytes,
            };
            off += p.size_bytes;
            e
        })
        .collect();
    let ntfs_off = entries[0].target_offset_bytes;
    let exfat_off = entries[1].target_offset_bytes;

    run(
        ClonePlan {
            entries,
            table_mode: CloneTableMode::ReinitMatchSource,
        },
        &src,
        &tgt,
        "full clone matching source style (MBR)",
    );

    let after = disk_by_index(tgt.index());
    assert!(
        !after.is_gpt,
        "ReinitMatchSource from an MBR source must leave the target MBR, got GPT"
    );

    verify_at(&tgt, ntfs_off, &ntfs_digest, true, "NTFS (MBR target)");
    verify_at(&tgt, exfat_off, &exfat_digest, false, "exFAT (MBR target)");
}

// ---------------------------------------------------------------------------
// 3. Clone with resize: NTFS grown into a larger slot, and shrunk into a smaller.
// ---------------------------------------------------------------------------

/// Clone the NTFS partition into a **larger** slot, so the volume must grow to
/// fill it (`FSCTL_EXTEND_VOLUME` after the data lands).
///
/// Deliberately grown into a bounded slot rather than "the rest of a 4 TB disk":
/// the extend is metadata-only and instant either way, but `chkdsk` on a 4 TB
/// volume is not, and an unbounded version of this test would simply never be run.
#[test]
#[ignore = "requires elevation + two real disks"]
fn real_clone_grow_ntfs_into_larger_slot() {
    let Some((src, tgt)) = rig() else { return };
    let (ntfs_digest, _) = build_source(&src);
    tgt.clean().expect("clean target");

    let parts = source_parts(&src);
    let ntfs = &parts[0];
    let grown = ntfs.size_bytes + 4096 * MB; // +4 GB
    let off = MB;

    run(
        ClonePlan {
            entries: vec![CloneEntry {
                source_partition_index: ntfs.index,
                target_offset_bytes: off,
                target_size_bytes: grown,
            }],
            table_mode: CloneTableMode::ReinitAs { gpt: true },
        },
        &src,
        &tgt,
        "clone NTFS into a larger slot (grow)",
    );

    let after = disk_by_index(tgt.index());
    let cloned = after
        .partitions
        .iter()
        .find(|p| p.offset_bytes == off)
        .expect("cloned partition missing from the table");
    assert_eq!(
        cloned.size_bytes, grown,
        "the partition table does not reflect the grown size"
    );
    verify_at(&tgt, off, &ntfs_digest, true, "grown NTFS");
}

/// Clone the NTFS partition into a **smaller** slot: the volume must shrink,
/// which runs the relocation map + the MFT / `$Bitmap` / `$LogFile` rewrite.
///
/// This is the most dangerous resize in the engine and the one most likely to
/// behave differently on real media, because real NTFS is fragmented and its
/// used clusters actually live near the end of the volume — which is precisely
/// what the relocation has to move, and precisely what an aligned, freshly
/// formatted VHDX fixture never has.
#[test]
#[ignore = "requires elevation + two real disks"]
fn real_clone_shrink_ntfs_into_smaller_slot() {
    let Some((src, tgt)) = rig() else { return };
    let (ntfs_digest, _) = build_source(&src);
    tgt.clean().expect("clean target");

    let parts = source_parts(&src);
    let ntfs = &parts[0];
    // Shrink to half. The fixture is far smaller than that, so its data fits —
    // but the volume's own metadata has to be relocated to make it so.
    let shrunk = ntfs.size_bytes / 2;
    let off = MB;

    run(
        ClonePlan {
            entries: vec![CloneEntry {
                source_partition_index: ntfs.index,
                target_offset_bytes: off,
                target_size_bytes: shrunk,
            }],
            table_mode: CloneTableMode::ReinitAs { gpt: true },
        },
        &src,
        &tgt,
        "clone NTFS into a smaller slot (shrink + relocation)",
    );

    let after = disk_by_index(tgt.index());
    let cloned = after
        .partitions
        .iter()
        .find(|p| p.offset_bytes == off)
        .expect("cloned partition missing from the table");
    assert_eq!(cloned.size_bytes, shrunk);
    verify_at(&tgt, off, &ntfs_digest, true, "shrunk NTFS");
}

// ---------------------------------------------------------------------------
// 4. Partial clone into a live target, preserving a sibling.
// ---------------------------------------------------------------------------

/// The Clone page's **default** mode, on real hardware: write one partition into
/// an existing slot of a live partition table, and leave the neighbour alone.
///
/// The target here is not a blank disk — it already holds two partitions with
/// their own data, both mounted, and the engine must lock and dismount only the
/// slot it is about to overwrite. Getting that wrong on a VHDX loses a fixture;
/// getting it wrong here is what losing real data looks like.
#[test]
#[ignore = "requires elevation + two real disks"]
fn real_partial_clone_preserves_the_sibling() {
    let Some((src, tgt)) = rig() else { return };
    let (ntfs_digest, exfat_digest) = build_source(&src);

    // Stage 1: full clone, so the target has a real, live, two-partition table.
    tgt.clean().expect("clean target");
    let parts = source_parts(&src);
    let ntfs_off = MB;
    let exfat_off = ntfs_off + parts[0].size_bytes;
    run(
        ClonePlan {
            entries: vec![
                CloneEntry {
                    source_partition_index: parts[0].index,
                    target_offset_bytes: ntfs_off,
                    target_size_bytes: parts[0].size_bytes,
                },
                CloneEntry {
                    source_partition_index: parts[1].index,
                    target_offset_bytes: exfat_off,
                    target_size_bytes: parts[1].size_bytes,
                },
            ],
            table_mode: CloneTableMode::ReinitAs { gpt: true },
        },
        &src,
        &tgt,
        "stage 1: full clone to set up a live target",
    );
    verify_at(&tgt, exfat_off, &exfat_digest, false, "stage 1 exFAT");

    // Stage 2: re-write ONLY the NTFS slot, preserving everything else.
    let live = disk_by_index(tgt.index());
    let slot = live
        .partitions
        .iter()
        .find(|p| p.offset_bytes == ntfs_off)
        .expect("NTFS slot missing on the target");
    let preserved: Vec<u32> = live
        .partitions
        .iter()
        .filter(|p| p.index != slot.index)
        .map(|p| p.index)
        .collect();
    eprintln!("[T3-clone] partial clone: preserving target partitions {preserved:?}");

    run(
        ClonePlan {
            entries: vec![CloneEntry {
                source_partition_index: parts[0].index,
                target_offset_bytes: ntfs_off,
                target_size_bytes: slot.size_bytes,
            }],
            table_mode: CloneTableMode::UpdateExisting {
                preserved_target_indices: preserved,
            },
        },
        &src,
        &tgt,
        "stage 2: PARTIAL clone into the live NTFS slot",
    );

    // The slot took the clone...
    verify_at(&tgt, ntfs_off, &ntfs_digest, true, "partially-cloned NTFS");
    // ...and the sibling the user never selected is still exactly itself. This is
    // the assertion the whole mode exists to not violate.
    verify_at(
        &tgt,
        exfat_off,
        &exfat_digest,
        false,
        "PRESERVED exFAT sibling",
    );

    let final_disk = disk_by_index(tgt.index());
    assert!(
        final_disk
            .partitions
            .iter()
            .any(|p| p.offset_bytes == exfat_off),
        "the preserved sibling fell off the partition table"
    );
}

// ---------------------------------------------------------------------------
// 5. Clone back the other way (big disk -> flash), i.e. a removable target.
// ---------------------------------------------------------------------------

/// Clone in reverse: from the big disk back onto the USB stick.
///
/// Users restore to smaller media all the time, and a removable target is a
/// genuinely different code path — Windows will not let it be GPT, and diskpart
/// refuses to format it, so the engine and the harness both take the MBR /
/// PowerShell routes. Worth proving the clone works in the direction that isn't
/// the demo.
#[test]
#[ignore = "requires elevation + two real disks"]
fn real_clone_back_from_hdd_to_flash() {
    let Some((src, tgt)) = rig() else { return };
    if !src.is_removable() {
        eprintln!("[T3-clone] SKIPPED: this test wants a removable source to clone back onto");
        return;
    }
    let (ntfs_digest, _) = build_source(&src);

    // Put the source's NTFS onto the big disk first...
    tgt.clean().expect("clean target");
    let parts = source_parts(&src);
    let off = MB;
    run(
        ClonePlan {
            entries: vec![CloneEntry {
                source_partition_index: parts[0].index,
                target_offset_bytes: off,
                target_size_bytes: parts[0].size_bytes,
            }],
            table_mode: CloneTableMode::ReinitAs { gpt: true },
        },
        &src,
        &tgt,
        "seed the HDD with the NTFS partition",
    );
    verify_at(&tgt, off, &ntfs_digest, true, "seeded NTFS on HDD");

    // ...then clone it back, HDD -> flash, wiping the flash to MBR.
    let hdd = disk_by_index(tgt.index());
    let hdd_ntfs = hdd
        .partitions
        .iter()
        .find(|p| p.offset_bytes == off)
        .expect("seeded partition missing");

    src.clean().expect("clean flash for the return trip");
    let back_off = MB;
    // NOTE: source and target swap roles here — the HDD is the source now.
    run_clone(CloneOptions {
        source_disk_index: tgt.index(),
        target_disk_index: src.index(),
        plan: ClonePlan {
            entries: vec![CloneEntry {
                source_partition_index: hdd_ntfs.index,
                target_offset_bytes: back_off,
                target_size_bytes: hdd_ntfs.size_bytes,
            }],
            // Removable media cannot be GPT, so the return trip must land on MBR.
            table_mode: CloneTableMode::ReinitAs { gpt: false },
        },
        verify: CloneVerify::ReadBack,
        convert_sector_size: false,
        progress: None,
    })
    .expect("clone back to the flash drive");

    let flash = disk_by_index(src.index());
    assert!(!flash.is_gpt, "removable media cannot be GPT");
    verify_at(
        &src,
        back_off,
        &ntfs_digest,
        true,
        "NTFS cloned back to flash",
    );
}
