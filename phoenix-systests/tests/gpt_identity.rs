//! Tier-2 GPT identity preservation: a restore must write back the source's
//! disk GUID, partition unique GUIDs, and attribute bits — the identities the
//! BCD uses to locate boot partitions.
//!
//! The collision case is deliberately AVOIDED here: Windows forbids duplicate
//! GPT identities among online disks, so restoring while the source is still
//! attached makes the OS regenerate the clone's GUIDs at online time (that
//! behavior is observed/tolerated in the T3B boot-disk tests). To prove the
//! engine actually writes the identities, this test DETACHES the source VHD
//! before restoring — no collision, so what our restore wrote is what
//! enumeration must read back.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests --test gpt_identity -- --ignored --test-threads=1 --nocapture

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, guid_from_string};
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture, wait_for_letter,
    wait_for_restored_letter, PartSpec, TestFs, TestVhd,
};

const VHD_MB: u64 = 512;

#[test]
#[ignore = "requires elevation + diskpart"]
fn gpt_identity_preserved_when_source_absent() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source: GPT NTFS volume with a fixture.
    let mut source = TestVhd::create(VHD_MB).expect("create source vhd");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "IDSRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source volume never mounted");
    let digest = fill_fixture('X', 0x1D1D).expect("fill fixture");

    let backup_path =
        std::env::temp_dir().join(format!("gptid-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: enumerate_disks()
            .unwrap()
            .iter()
            .find(|d| d.index == source.disk_index())
            .unwrap()
            .partitions
            .iter()
            .map(|p| p.index)
            .collect(),
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");

    // The manifest must carry the identities...
    let reader = PhnxReader::open(&backup_path).unwrap();
    let manifest_disk_guid = reader
        .manifest
        .disk
        .disk_guid
        .clone()
        .expect("manifest missing disk GUID");
    let expected: Vec<(u32, [u8; 16], u64)> = reader
        .manifest
        .partitions
        .iter()
        .map(|p| {
            (
                p.index,
                p.unique_guid
                    .as_deref()
                    .and_then(guid_from_string)
                    .unwrap_or_else(|| panic!("partition {} missing unique_guid", p.index)),
                p.gpt_attributes
                    .unwrap_or_else(|| panic!("partition {} missing gpt_attributes", p.index)),
            )
        })
        .collect();

    // ...and the SOURCE must be gone before the restore, so no GUID collides
    // and Windows has no reason to regenerate anything.
    source.detach().expect("detach source vhd");

    let target = TestVhd::create(VHD_MB).expect("create target vhd");
    let plan = default_plan_from_backup(
        backup_path.to_str().unwrap(),
        &reader,
        target.disk_index(),
        VHD_MB * 1024 * 1024,
    );
    drop(reader);
    run_restore(RestoreOptions {
        backup_path: backup_path.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    // Restored disk must carry the source's exact identities.
    let live = enumerate_disks()
        .unwrap()
        .into_iter()
        .find(|d| d.index == target.disk_index())
        .expect("target disk vanished");
    assert!(live.is_gpt);
    let live_disk_guid = live.disk_guid.expect("restored disk has no GPT GUID");
    assert_eq!(
        phoenix_core::disk::guid_to_string(&live_disk_guid),
        manifest_disk_guid,
        "restored disk GUID does not match the source"
    );
    assert_eq!(live.partitions.len(), expected.len());
    for (pos, (idx, want_guid, want_attrs)) in expected.iter().enumerate() {
        let lp = &live.partitions[pos];
        assert_eq!(
            lp.unique_guid, *want_guid,
            "partition {idx} unique GUID not preserved (no collision was present)"
        );
        assert_eq!(
            lp.gpt_attributes, *want_attrs,
            "partition {idx} GPT attributes not preserved"
        );
    }

    // And the data still round-trips.
    let letter = wait_for_restored_letter(target.disk_index(), 30_000)
        .expect("restored volume got no letter");
    verify_fixture(letter, &digest).expect("fixture preserved");

    let _ = std::fs::remove_file(&backup_path);
    eprintln!("[gpt-identity] disk GUID + partition unique GUIDs + attributes preserved");
}
