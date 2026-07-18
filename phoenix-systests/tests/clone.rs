//! Tier-2 system test for disk-to-disk cloning: fill a source VHDX, clone it
//! to a target VHDX, and assert the target's filesystem is consistent and its
//! files are byte-identical.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests -- --ignored --test-threads=1

use phoenix_clone::{run_clone, CloneOptions, ClonePlan, CloneVerify};
use phoenix_core::disk::{enumerate_disks, refine_partition_fs};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture,
    wait_for_letter, wait_for_restored_letter, PartSpec, TestFs, TestVhd,
};

#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_clone_same_size() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source: 512 MiB, one NTFS partition, filled.
    let source = TestVhd::create(512).expect("create source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let digest = fill_fixture('X', 0x5151).expect("fill fixture");

    // Target: same size, uninitialized.
    let target = TestVhd::create(512).expect("create target");

    // Re-enumerate so the engine sees both attached disks.
    let mut disks = enumerate_disks().unwrap();
    for d in &mut disks {
        for p in &mut d.partitions {
            refine_partition_fs(p);
        }
    }
    let src_disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap()
        .clone();
    let tgt_disk = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .unwrap()
        .clone();

    let plan = ClonePlan::identity(&src_disk);
    plan.validate(&src_disk, &tgt_disk).expect("plan valid");

    run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: target.disk_index(),
        plan,
        verify: CloneVerify::ReadBack,
        convert_sector_size: false,
        repair_boot: false,
        progress: None,
    })
    .expect("run_clone");

    let letter =
        wait_for_restored_letter(target.disk_index(), 30_000).expect("cloned volume got no letter");
    chkdsk_clean(letter).expect("chkdsk clean on cloned volume");
    verify_fixture(letter, &digest).expect("fixture bytes preserved by clone");
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_clone_expand_to_larger_target() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create(256).expect("create source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let digest = fill_fixture('X', 0x6262).expect("fill fixture");

    // Larger target so expand_to_fill grows the NTFS partition.
    let target = TestVhd::create(768).expect("create target");

    let mut disks = enumerate_disks().unwrap();
    for d in &mut disks {
        for p in &mut d.partitions {
            refine_partition_fs(p);
        }
    }
    let src_disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap()
        .clone();
    let tgt_disk = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .unwrap()
        .clone();

    let plan = ClonePlan::expand_to_fill(&src_disk, &tgt_disk);
    plan.validate(&src_disk, &tgt_disk).expect("plan valid");

    run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: target.disk_index(),
        plan,
        verify: CloneVerify::None,
        convert_sector_size: false,
        repair_boot: false,
        progress: None,
    })
    .expect("run_clone expand");

    let letter =
        wait_for_restored_letter(target.disk_index(), 30_000).expect("cloned volume got no letter");
    chkdsk_clean(letter).expect("chkdsk clean");
    verify_fixture(letter, &digest).expect("fixture preserved after expand-clone");
}
