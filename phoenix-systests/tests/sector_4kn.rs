//! Tier 2-4Kn — system tests on a **true 4Kn** virtual disk (4096-byte logical
//! sectors).
//!
//! Every other T2 fixture is 512-byte-sector, because diskpart's `create vdisk`
//! cannot set a sector size. `TestVhd::create_4kn` builds the VHDX through
//! `CreateVirtualDisk` (virtdisk.dll — core Windows, no Hyper-V, no Pro SKU)
//! with an explicit `SectorSizeInBytes`, which is the only way to get a 4Kn disk
//! without 4Kn hardware.
//!
//! This is the tier that covers the ARM64 laptop's UFS drive *if* it turns out
//! to report 4096-byte logical sectors — and it covers it here, on x64, without
//! that hardware.
//!
//! ```text
//! cargo test -p phoenix-systests --test sector_4kn -- --ignored --test-threads=1 --nocapture
//! ```

use phoenix_core::disk::enumerate_disks;
use phoenix_systests::{cleanup_leaked_vhds, require_admin, TestVhd};

/// The disk really is 4Kn, and our own enumeration agrees.
///
/// This is the foundation the rest of the tier stands on: if `get_sector_size`
/// doesn't report 4096 here, every downstream 4Kn assertion is meaningless.
#[test]
#[ignore = "requires elevation + diskpart"]
fn vhdx_4kn_reports_4096_byte_sectors() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let vhd = TestVhd::create_4kn(2048).expect("create 4Kn VHDX");

    let disks = enumerate_disks().expect("enumerate_disks");
    let d = disks
        .iter()
        .find(|d| d.index == vhd.disk_index())
        .unwrap_or_else(|| panic!("disk {} not enumerated", vhd.disk_index()));

    assert_eq!(
        d.sector_size, 4096,
        "CreateVirtualDisk was asked for a 4096-byte logical sector, but the \
         attached disk enumerates as {} — the fixture itself is wrong, so no \
         4Kn conclusion drawn from it would be trustworthy",
        d.sector_size
    );
}
