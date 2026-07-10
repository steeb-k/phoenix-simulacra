//! Tier-3B — BOOT-DISK cloning against this machine's LIVE system disk.
//!
//! The source disk is only ever READ (live VSS capture); every destructive
//! operation targets the opt-in test disk behind the standard `RealDisk`
//! safety gate, which structurally refuses the boot/system disk. The image
//! file must live on a volume that is on NEITHER the source nor the target
//! disk (a restore would otherwise wipe the image out from under itself).
//!
//! Environment:
//! ```text
//! $env:PHOENIX_BOOT_SRC_DISK = "0"                  # disk to image (READ-ONLY)
//! $env:PHOENIX_BOOT_IMG_DIR  = "D:\phoenix-boot"    # where the .phnx lives
//! # target disk for the restore tests (standard T3 gate; non-removable needs:)
//! $env:PHOENIX_T3_DISK        = "3"
//! $env:PHOENIX_T3_ALLOW_FIXED = "1"
//! $env:PHOENIX_T3_SERIAL      = "<serial>"
//! $env:PHOENIX_T3_MAX_GB      = "4100"
//! ```
//!
//! Run stages in order (they sort alphabetically; capture is the slow one):
//! ```text
//! cargo test -p phoenix-systests --test boot_disk -- --ignored --test-threads=1 --nocapture
//! # or one stage:
//! cargo test -p phoenix-systests --test boot_disk boot_a -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Stages:
//!   a. capture   — live VSS image of the source disk → .phnx (+ full verify)
//!   b. as-is     — full-disk restore, every partition at its ORIGINAL size
//!   c. shrink    — main NTFS shrunk (relocation); all other sizes preserved
//!   d. grow      — main NTFS expanded; trailing partitions moved, sizes kept
//!   e. partial   — re-plop ONLY the NTFS partition over its existing slot
//!
//! Each restore asserts: target is GPT, per-partition type GUIDs + unique
//! GUIDs + attribute bits match the manifest (boot-clone fidelity), boot
//! artifacts exist (\Windows\System32\ntoskrnl.exe, config\SYSTEM, and the
//! ESP's EFI\Microsoft\Boot\BCD), and the restored NTFS passes an offline
//! chkdsk. Actually BOOTING the clone stays a manual step (swap the drive) —
//! that can't be automated safely from a running OS.

use std::path::{Path, PathBuf};

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, guid_from_string, DiskInfo};
use phoenix_restore::plan::{
    align_up, build_partial_plan, default_plan_from_backup, RestorePlan, RestorePlanEntry,
};
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_offline_fix, powershell, require_admin, wait_for_disk_volumes, ConsoleProgress, RealDisk,
};

const ALIGN: u64 = 1024 * 1024;

/// Wall-clock for the whole stage (engine ops get their own per-op timing
/// from `ConsoleProgress`; this also covers chkdsk, diskpart, and waits).
fn stage_elapsed(start: std::time::Instant) -> String {
    phoenix_core::progress::format_elapsed(start.elapsed().as_secs_f64())
}

// ---------- environment / safety plumbing ----------

fn src_disk_index() -> Option<u32> {
    std::env::var("PHOENIX_BOOT_SRC_DISK")
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn img_dir() -> Option<PathBuf> {
    std::env::var("PHOENIX_BOOT_IMG_DIR")
        .ok()
        .map(PathBuf::from)
}

/// `PHOENIX_BOOT_SKIP_RESTORE=1` re-runs ONLY a stage's post-restore
/// assertions against whatever is already on the target disk — for finishing
/// a stage whose multi-hour restore succeeded but whose checks tripped.
/// Only meaningful when the disk currently holds THAT stage's layout.
fn skip_restore() -> bool {
    std::env::var("PHOENIX_BOOT_SKIP_RESTORE")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Default image path; `PHOENIX_BOOT_IMAGE` overrides (lets restore stages
/// reuse an image captured earlier without re-imaging the source).
fn image_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PHOENIX_BOOT_IMAGE") {
        return Some(PathBuf::from(p));
    }
    let src = src_disk_index()?;
    Some(img_dir()?.join(format!("bootdisk-{src}.phnx")))
}

fn source_disk(src: u32) -> DiskInfo {
    enumerate_disks()
        .unwrap()
        .into_iter()
        .find(|d| d.index == src)
        .unwrap_or_else(|| panic!("source disk {src} not found"))
}

/// The disk number hosting a path's drive letter (for the image-location
/// safety check).
fn disk_of_path(path: &Path) -> Option<u32> {
    let letter = path
        .to_str()?
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())?;
    let out = powershell(&format!(
        "(Get-Partition -DriveLetter {letter} -ErrorAction Stop).DiskNumber"
    ))
    .ok()?;
    out.trim().parse().ok()
}

/// Acquire the restore target via the standard destructive-op gate (refuses
/// boot/system disks) and cross-check it against the source and image path.
fn acquire_target(src: u32, image: &Path) -> Option<RealDisk> {
    let disk = match RealDisk::acquire() {
        Ok(Some(d)) => d,
        Ok(None) => {
            eprintln!("[T3B] PHOENIX_T3_DISK not set — skipping restore stage");
            return None;
        }
        Err(e) => panic!("target safety check failed: {e:#}"),
    };
    assert_ne!(
        disk.index(),
        src,
        "SAFETY: target disk == source disk — refusing"
    );
    if let Some(img_disk) = disk_of_path(image) {
        assert_ne!(
            img_disk,
            disk.index(),
            "SAFETY: the image file lives on the target disk — the restore would wipe it"
        );
    }
    Some(disk)
}

// ---------- shared assertions ----------

/// Largest NTFS partition in the backup = the OS volume.
fn main_ntfs(reader: &PhnxReader) -> (u32, u64, u64) {
    reader
        .manifest
        .partitions
        .iter()
        .filter(|p| p.fs == "ntfs")
        .max_by_key(|p| p.original_size)
        .map(|p| (p.index, p.original_size, p.used_bytes))
        .expect("backup contains no NTFS partition")
}

/// Front-pack every backup partition at 1 MiB alignment in index order with
/// per-partition target sizes. (The .phnx format records sizes, not source
/// offsets, so "as-is" means order + exact sizes; offsets normalize to the
/// standard 1 MiB-aligned layout Windows itself uses.)
fn front_pack_plan(
    backup: &Path,
    reader: &PhnxReader,
    target_disk: u32,
    size_for: impl Fn(u32, u64) -> u64,
) -> RestorePlan {
    let mut entries = Vec::new();
    let mut offset = ALIGN;
    for e in &reader.index {
        let size = size_for(e.index, e.original_size);
        let off = align_up(offset, ALIGN);
        entries.push(RestorePlanEntry {
            source_partition_index: Some(e.index),
            target_partition_index: e.index,
            restore: true,
            target_offset_bytes: off,
            target_size_bytes: size,
        });
        offset = off + size;
    }
    RestorePlan {
        backup_path: backup.to_string_lossy().into_owned(),
        target_disk_index: target_disk,
        entries,
        full_disk: true,
    }
}

fn run_restore_plan(backup: &Path, plan: RestorePlan) {
    let progress = ConsoleProgress::start("restore");
    run_restore(RestoreOptions {
        backup_path: backup.to_path_buf(),
        plan,
        verify_on_restore: true,
        progress: Some(progress.handle()),
    })
    .expect("run_restore");
}

/// Post-restore fidelity: target is GPT and every restored partition's type
/// GUID, unique GUID, and attribute bits match the manifest. Returns the live
/// view for further checks.
///
/// Unique-GUID caveat (learned the 5-hour way): Windows FORBIDS duplicate GPT
/// identities among online disks. When the SOURCE disk is still attached —
/// the normal situation for a same-machine clone test — bringing the restored
/// disk online makes Windows regenerate its disk GUID and every colliding
/// partition GUID (observed as one sequential v1-GUID batch). The engine
/// wrote the source GUIDs; the OS deduplicated them. So a unique-GUID
/// mismatch is only a FAILURE when the expected GUID is NOT held by another
/// online disk; when it is, dedup is the correct outcome and we warn.
/// Collision-free preservation is proven separately in T2
/// (`gpt_identity_preserved_when_source_absent`).
fn assert_gpt_fidelity(target: u32, reader: &PhnxReader) -> DiskInfo {
    let disks = enumerate_disks().unwrap();
    // Every partition unique GUID present on OTHER online disks — the
    // collision set that forces Windows to regenerate on the clone.
    let other_disk_guids: Vec<[u8; 16]> = disks
        .iter()
        .filter(|d| d.index != target)
        .flat_map(|d| d.partitions.iter().map(|p| p.unique_guid))
        .collect();
    let live = disks
        .into_iter()
        .find(|d| d.index == target)
        .expect("target disk vanished after restore");
    assert!(
        live.is_gpt,
        "restored disk is NOT GPT — a GPT image must always restore as GPT"
    );
    assert_eq!(
        live.partitions.len(),
        reader.index.len(),
        "partition count changed across restore"
    );

    // Live enumeration orders by on-disk offset; our plans keep index order,
    // so zip positionally and compare identities.
    for (pos, idx_entry) in reader.index.iter().enumerate() {
        let lp = &live.partitions[pos];
        let m = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.index == idx_entry.index)
            .unwrap();
        assert_eq!(
            lp.type_guid, idx_entry.type_guid,
            "partition {} type GUID mismatch",
            idx_entry.index
        );
        if let Some(want_attrs) = m.gpt_attributes {
            assert_eq!(
                lp.gpt_attributes, want_attrs,
                "partition {} GPT attributes mismatch (Recovery hidden/no-automount bits!)",
                idx_entry.index
            );
        }
        if let Some(want_guid) = m.unique_guid.as_deref().and_then(guid_from_string) {
            if lp.unique_guid != want_guid {
                if other_disk_guids.contains(&want_guid) {
                    eprintln!(
                        "[T3B] partition {}: unique GUID regenerated by Windows (the source \
                         disk holding the original GUID is attached — dedup is mandatory). \
                         NOTE: the clone's BCD still references the ORIGINAL GUIDs; booting \
                         it needs the source detached or a BCD fixup (see ROADMAP).",
                        idx_entry.index
                    );
                } else {
                    panic!(
                        "partition {} unique GUID mismatch with NO collision present — the \
                         restore failed to write the source identity (BCD device references \
                         would break)",
                        idx_entry.index
                    );
                }
            }
        }
        eprintln!(
            "[T3B] partition {}: {} — size {} attrs {:#x} ok",
            idx_entry.index, m.name, lp.size_bytes, lp.gpt_attributes
        );
    }
    live
}

/// The Windows-boot artifact checks: OS files on the restored NTFS and the
/// BCD on the restored ESP.
fn assert_boot_artifacts(target: u32) {
    // Give mountmgr a moment, then find the NTFS volume's letter.
    let letters = wait_for_disk_volumes(target, 1, 60_000).expect("no volume mounted on target");
    // The OS volume is the (only) large lettered one; probe each letter for
    // \Windows to be robust against ESP/recovery also getting letters.
    let os_letter = letters
        .iter()
        .copied()
        .find(|l| Path::new(&format!("{l}:\\Windows\\System32")).exists())
        .unwrap_or_else(|| {
            panic!("no restored volume contains \\Windows\\System32 (letters: {letters:?})")
        });
    for probe in [
        "Windows\\System32\\ntoskrnl.exe",
        "Windows\\System32\\config\\SYSTEM",
    ] {
        let p = format!("{os_letter}:\\{probe}");
        assert!(
            Path::new(&p).exists(),
            "boot artifact missing after restore: {p}"
        );
    }
    eprintln!("[T3B] OS volume {os_letter}: has ntoskrnl.exe + SYSTEM hive");

    // ESP check: temporarily give the System partition a letter and look for
    // the BCD. Best-effort — some environments refuse ESP letter assignment.
    let esp_check = powershell(&format!(
        "$ErrorActionPreference='Stop'; \
         $p = Get-Partition -DiskNumber {target} | Where-Object {{ $_.Type -eq 'System' }} | Select-Object -First 1; \
         if (-not $p) {{ 'NO-ESP'; exit 0 }}; \
         Add-PartitionAccessPath -DiskNumber {target} -PartitionNumber $p.PartitionNumber -AssignDriveLetter | Out-Null; \
         $l = (Get-Partition -DiskNumber {target} -PartitionNumber $p.PartitionNumber).DriveLetter; \
         $bcd = Test-Path (\"$($l):\\EFI\\Microsoft\\Boot\\BCD\"); \
         Remove-PartitionAccessPath -DiskNumber {target} -PartitionNumber $p.PartitionNumber -AccessPath (\"$($l):\\\") -ErrorAction SilentlyContinue; \
         if ($bcd) {{ 'BCD-OK' }} else {{ 'BCD-MISSING' }}"
    ));
    match esp_check.as_deref().map(str::trim) {
        Ok("BCD-OK") => eprintln!("[T3B] ESP contains EFI\\Microsoft\\Boot\\BCD"),
        Ok("NO-ESP") => panic!("restored disk has no EFI System partition"),
        Ok("BCD-MISSING") => panic!("ESP restored but EFI\\Microsoft\\Boot\\BCD is missing"),
        Ok(other) => eprintln!("[T3B] ESP check inconclusive ({other}); continuing"),
        Err(e) => eprintln!("[T3B] ESP letter assignment failed ({e:#}); skipping BCD check"),
    }

    chkdsk_offline_fix(os_letter).expect("offline chkdsk on restored OS volume");
}

// ---------- stages ----------

/// Stage A: live VSS image of the source disk. READ-ONLY on the source.
#[test]
#[ignore = "images the LIVE source disk (read-only); set PHOENIX_BOOT_SRC_DISK + PHOENIX_BOOT_IMG_DIR"]
fn boot_a_capture_image() {
    require_admin();
    let stage_start = std::time::Instant::now();
    let Some(src) = src_disk_index() else {
        eprintln!("[T3B] PHOENIX_BOOT_SRC_DISK not set — skipping");
        return;
    };
    let Some(dir) = img_dir() else {
        eprintln!("[T3B] PHOENIX_BOOT_IMG_DIR not set — skipping");
        return;
    };
    std::fs::create_dir_all(&dir).expect("create image dir");
    let image = image_path().unwrap();

    let disk = source_disk(src);
    assert!(disk.is_gpt, "source disk {src} is not GPT");
    if let Some(img_disk) = disk_of_path(&image) {
        assert_ne!(
            img_disk, src,
            "image dir is on the source disk — put it on another volume (it would balloon \
             the VSS snapshot and the image would contain a partial copy of itself)"
        );
    }
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let used_estimate: u64 = disk
        .partitions
        .iter()
        .map(|p| p.usage.map(|u| u.used_bytes()).unwrap_or(p.size_bytes))
        .sum();
    eprintln!(
        "[T3B] imaging disk {src} ({} partitions, ~{:.1} GB used) -> {} — this reads the used \
         data TWICE (capture + verify-after); expect it to take a while and print nothing",
        parts.len(),
        used_estimate as f64 / 1e9,
        image.display()
    );

    let progress = ConsoleProgress::start("capture");
    run_backup(BackupOptions {
        disk_index: src,
        partition_indices: parts,
        output: image.clone(),
        use_vss: true,
        verify_after: true,
        progress: Some(progress.handle()),
    })
    .expect("live VSS capture of the source disk failed");
    drop(progress);

    // Independent full verification of the image file itself.
    let mut reader = PhnxReader::open(&image).expect("open image");
    let vprogress = ConsoleProgress::start("verify-image");
    reader
        .verify_all_with_progress(false, Some(vprogress.handle()))
        .expect("image fails full verify");
    drop(vprogress);

    eprintln!("[T3B] image verified. Manifest:");
    assert_eq!(reader.manifest.disk.style, "gpt");
    for p in &reader.manifest.partitions {
        eprintln!(
            "[T3B]   [{}] {:<20} fs={:<9} mode={:<11} size={:>14} used={:>14} attrs={:?} guid={:?}",
            p.index,
            p.name,
            p.fs,
            p.capture_mode,
            p.original_size,
            p.used_bytes,
            p.gpt_attributes,
            p.unique_guid
        );
        // Boot-clone fidelity must be present for every GPT partition.
        assert!(
            p.unique_guid.is_some(),
            "partition {} missing unique_guid in manifest — fidelity capture broke",
            p.index
        );
        assert!(p.gpt_attributes.is_some());
    }
    let (ntfs_idx, ntfs_size, ntfs_used) = main_ntfs(&reader);
    eprintln!(
        "[T3B] main NTFS = partition {ntfs_idx} ({:.1} GB, {:.1} GB used)",
        ntfs_size as f64 / 1e9,
        ntfs_used as f64 / 1e9
    );
    eprintln!(
        "[T3B] boot_a_capture_image PASSED in {} — image at {}",
        stage_elapsed(stage_start),
        image.display()
    );
}

/// Stage B: full-disk restore with every partition at its original size.
#[test]
#[ignore = "DESTRUCTIVE to the target disk; needs the stage-A image + T3 target env"]
fn boot_b_restore_asis() {
    require_admin();
    let stage_start = std::time::Instant::now();
    let (Some(src), Some(image)) = (src_disk_index(), image_path()) else {
        eprintln!("[T3B] source/image env not set — skipping");
        return;
    };
    if !image.exists() {
        eprintln!("[T3B] image {} missing — run boot_a first", image.display());
        return;
    }
    let Some(target) = acquire_target(src, &image) else {
        return;
    };

    let reader = PhnxReader::open(&image).unwrap();
    let plan = front_pack_plan(&image, &reader, target.index(), |_, orig| orig);
    let target_size = enumerate_disks()
        .unwrap()
        .iter()
        .find(|d| d.index == target.index())
        .unwrap()
        .size_bytes;
    plan.validate_layout(target_size)
        .expect("as-is layout does not fit the target disk");
    if skip_restore() {
        eprintln!(
            "[T3B] PHOENIX_BOOT_SKIP_RESTORE=1 — validating the EXISTING layout on disk {} \
             (it must already hold THIS stage's restore)",
            target.index()
        );
    } else {
        target.clean().expect("clean target");
        eprintln!(
            "[T3B] restoring AS-IS (all original sizes) to disk {}",
            target.index()
        );
        run_restore_plan(&image, plan);
    }

    let live = assert_gpt_fidelity(target.index(), &reader);
    // As-is: every partition keeps its exact original size.
    for (pos, idx_entry) in reader.index.iter().enumerate() {
        assert_eq!(
            live.partitions[pos].size_bytes, idx_entry.original_size,
            "partition {} size changed in an as-is restore",
            idx_entry.index
        );
    }
    assert_boot_artifacts(target.index());
    eprintln!(
        "[T3B] boot_b_restore_asis PASSED in {}",
        stage_elapsed(stage_start)
    );
}

/// Stage C: main NTFS SHRUNK (relocation path); every other partition keeps
/// its exact original size and follows in order.
#[test]
#[ignore = "DESTRUCTIVE to the target disk; needs the stage-A image + T3 target env"]
fn boot_c_restore_shrink() {
    require_admin();
    let stage_start = std::time::Instant::now();
    let (Some(src), Some(image)) = (src_disk_index(), image_path()) else {
        eprintln!("[T3B] source/image env not set — skipping");
        return;
    };
    if !image.exists() {
        eprintln!("[T3B] image {} missing — run boot_a first", image.display());
        return;
    }
    let Some(target) = acquire_target(src, &image) else {
        return;
    };

    let mut reader = PhnxReader::open(&image).unwrap();
    let (ntfs_idx, ntfs_orig, ntfs_used) = main_ntfs(&reader);
    // used + 25% headroom, 1 MiB aligned — a genuine shrink that still leaves
    // the relocator room to pack displaced clusters.
    let shrunk = align_up((ntfs_used as f64 * 1.25) as u64, ALIGN).min(ntfs_orig);
    assert!(
        shrunk < ntfs_orig,
        "computed shrink target {shrunk} is not smaller than the original {ntfs_orig} — \
         volume too full to shrink meaningfully; skip this stage"
    );
    eprintln!(
        "[T3B] shrinking NTFS partition {ntfs_idx}: {:.1} GB -> {:.1} GB (used {:.1} GB)",
        ntfs_orig as f64 / 1e9,
        shrunk as f64 / 1e9,
        ntfs_used as f64 / 1e9
    );

    let plan = front_pack_plan(&image, &reader, target.index(), |idx, orig| {
        if idx == ntfs_idx {
            shrunk
        } else {
            orig
        }
    });
    plan.validate_extents_fit(&mut reader)
        .expect("shrink target too small for the relocation map");
    if skip_restore() {
        eprintln!("[T3B] PHOENIX_BOOT_SKIP_RESTORE=1 — validating the EXISTING (shrunk) layout");
    } else {
        target.clean().expect("clean target");
        run_restore_plan(&image, plan);
    }

    let live = assert_gpt_fidelity(target.index(), &reader);
    for (pos, idx_entry) in reader.index.iter().enumerate() {
        let want = if idx_entry.index == ntfs_idx {
            shrunk
        } else {
            idx_entry.original_size
        };
        assert_eq!(
            live.partitions[pos].size_bytes, want,
            "partition {} size wrong after shrink restore",
            idx_entry.index
        );
    }
    assert_boot_artifacts(target.index());
    eprintln!(
        "[T3B] boot_c_restore_shrink PASSED in {}",
        stage_elapsed(stage_start)
    );
}

/// Stage D: main NTFS GROWN via the default plan (trailing partitions move to
/// the disk tail at their original sizes). `PHOENIX_T3_LAYOUT_GB` caps how far
/// the layout extends into a huge target.
#[test]
#[ignore = "DESTRUCTIVE to the target disk; needs the stage-A image + T3 target env"]
fn boot_d_restore_grow() {
    require_admin();
    let stage_start = std::time::Instant::now();
    let (Some(src), Some(image)) = (src_disk_index(), image_path()) else {
        eprintln!("[T3B] source/image env not set — skipping");
        return;
    };
    if !image.exists() {
        eprintln!("[T3B] image {} missing — run boot_a first", image.display());
        return;
    }
    let Some(target) = acquire_target(src, &image) else {
        return;
    };

    let reader = PhnxReader::open(&image).unwrap();
    let (ntfs_idx, ntfs_orig, _) = main_ntfs(&reader);
    let disk_size = enumerate_disks()
        .unwrap()
        .iter()
        .find(|d| d.index == target.index())
        .unwrap()
        .size_bytes;
    let layout_size = std::env::var("PHOENIX_T3_LAYOUT_GB")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|gb| ((gb * 1e9) as u64).min(disk_size))
        .unwrap_or(disk_size);
    let plan = default_plan_from_backup(
        image.to_str().unwrap(),
        &reader,
        target.index(),
        layout_size,
    );
    let planned_ntfs = plan
        .entries
        .iter()
        .find(|e| e.source_partition_index == Some(ntfs_idx))
        .unwrap()
        .target_size_bytes;
    assert!(
        planned_ntfs > ntfs_orig,
        "default plan did not grow the NTFS ({planned_ntfs} <= {ntfs_orig}); \
         raise PHOENIX_T3_LAYOUT_GB"
    );
    eprintln!(
        "[T3B] growing NTFS partition {ntfs_idx}: {:.1} GB -> {:.1} GB",
        ntfs_orig as f64 / 1e9,
        planned_ntfs as f64 / 1e9
    );
    if skip_restore() {
        eprintln!("[T3B] PHOENIX_BOOT_SKIP_RESTORE=1 — validating the EXISTING (grown) layout");
    } else {
        target.clean().expect("clean target");
        run_restore_plan(&image, plan);
    }

    let live = assert_gpt_fidelity(target.index(), &reader);
    // Grown NTFS + every non-NTFS partition at its original size (trailing
    // ones may have MOVED, but must not have resized).
    for (pos, idx_entry) in reader.index.iter().enumerate() {
        let lp = &live.partitions[pos];
        if idx_entry.index == ntfs_idx {
            assert!(
                lp.size_bytes > ntfs_orig,
                "NTFS did not actually grow on disk"
            );
        } else {
            assert_eq!(
                lp.size_bytes, idx_entry.original_size,
                "non-NTFS partition {} resized during a grow restore",
                idx_entry.index
            );
        }
    }
    assert_boot_artifacts(target.index());
    eprintln!(
        "[T3B] boot_d_restore_grow PASSED in {}",
        stage_elapsed(stage_start)
    );
}

/// Stage E: PARTIAL restore — plop only the NTFS partition from the image
/// over the existing NTFS slot on the target (which must already hold a
/// restored layout from stage B/C/D). Everything else must be untouched.
#[test]
#[ignore = "DESTRUCTIVE to the target's NTFS slot; run after boot_b/c/d"]
fn boot_e_partial_ntfs() {
    require_admin();
    let stage_start = std::time::Instant::now();
    let (Some(src), Some(image)) = (src_disk_index(), image_path()) else {
        eprintln!("[T3B] source/image env not set — skipping");
        return;
    };
    if !image.exists() {
        eprintln!("[T3B] image {} missing — run boot_a first", image.display());
        return;
    }
    let Some(target) = acquire_target(src, &image) else {
        return;
    };

    let reader = PhnxReader::open(&image).unwrap();
    let (ntfs_idx, _, ntfs_used) = main_ntfs(&reader);

    // The target must already carry a GPT layout with an NTFS slot big enough.
    let live_before = enumerate_disks()
        .unwrap()
        .into_iter()
        .find(|d| d.index == target.index())
        .unwrap();
    if !live_before.is_gpt || live_before.partitions.len() < 2 {
        eprintln!("[T3B] target has no restored GPT layout — run boot_b/c/d first; skipping");
        return;
    }
    let slot = live_before
        .partitions
        .iter()
        .filter(|p| p.size_bytes >= ntfs_used)
        .max_by_key(|p| p.size_bytes)
        .expect("no target slot large enough for the NTFS used data")
        .clone();
    eprintln!(
        "[T3B] partial restore: image partition {ntfs_idx} -> target slot {} \
         (offset {}, {:.1} GB)",
        slot.index,
        slot.offset_bytes,
        slot.size_bytes as f64 / 1e9
    );

    let plan = build_partial_plan(
        image.to_str().unwrap(),
        target.index(),
        &live_before,
        &[(slot.index, ntfs_idx, slot.offset_bytes, slot.size_bytes)],
    );
    run_restore_plan(&image, plan);

    // Untouched partitions: identical offset/size/type/unique-GUID before vs after.
    let live_after = enumerate_disks()
        .unwrap()
        .into_iter()
        .find(|d| d.index == target.index())
        .unwrap();
    assert!(
        live_after.is_gpt,
        "partial restore must not change GPT-ness"
    );
    assert_eq!(live_after.partitions.len(), live_before.partitions.len());
    for (b, a) in live_before
        .partitions
        .iter()
        .zip(live_after.partitions.iter())
        .filter(|(b, _)| b.index != slot.index)
    {
        assert_eq!(b.offset_bytes, a.offset_bytes, "untouched partition moved");
        assert_eq!(b.size_bytes, a.size_bytes, "untouched partition resized");
        assert_eq!(b.type_guid, a.type_guid, "untouched partition re-typed");
        assert_eq!(
            b.unique_guid, a.unique_guid,
            "untouched partition lost its unique GUID"
        );
    }
    assert_boot_artifacts(target.index());
    eprintln!(
        "[T3B] boot_e_partial_ntfs PASSED in {}",
        stage_elapsed(stage_start)
    );
}
