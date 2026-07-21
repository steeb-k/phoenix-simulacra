//! Feasibility spike (Phase 1, approach A): a **writable overlay mount** via a
//! Windows-managed differencing VHDX over our synthesized parent.
//!
//! The thesis: mount a backup read/write without ever touching the immutable,
//! hash-sealed `.phnx`. Windows does the copy-on-write into a child `.avhdx`; the
//! backup stays pristine, so all validation still holds and the footprint grows
//! only with writes.
//!
//! What this test proves (or kills), cheaply and with no production changes:
//!
//!   1. `CreateVirtualDisk` accepts **our** synthesized VHDX as a differencing
//!      *parent* — the sharpest unknown (the parent-locator / VirtualDiskID
//!      handshake). `phoenix_mount::vhdx` already writes a VirtualDiskID metadata
//!      item, so it *should* qualify; this confirms it against real Windows.
//!   2. The differencing child attaches **read-write** and its NTFS volume mounts.
//!   3. Existing backup data reads back correctly *through* the diff (parent
//!      reads), and a brand-new write persists in the child (CoW writes).
//!   4. The **parent is byte-identical** afterward — i.e. in the real feature the
//!      on-demand `.phnx` synthesis would never be written, so the backup is safe.
//!   5. The backing `.phnx` still passes a full `verify_all`.
//!
//! This deliberately uses a **materialized** parent file rather than the
//! WinFsp-served one: the existing `winfsp_mount` test already proves
//! `AttachVirtualDisk` works over a WinFsp-served file, so serving the parent on
//! demand is a separate, lower-risk step. Here we isolate the differencing
//! mechanism itself.
//!
//! A 4Kn source is used only because our container chooses VHDX for 4Kn (a fixed
//! VHD cannot be a VHDX differencing parent); the real feature would force VHDX
//! for 512e too. The 4Kn attach/mount path is already covered green by
//! `winfsp_mount_a_4kn_backup`, so nothing here is novel except the diff chain.
//!
//! Requires: elevated shell. No WinFsp, no `--features winfsp`, no libclang.
//!   cargo test -p phoenix-systests --test rw_overlay_spike -- \
//!       --ignored --test-threads=1 --nocapture

use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::SyntheticVhd;
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture, wait_for_letter, PartSpec,
    TestFs, TestVhd,
};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::Vhd::{
    AttachVirtualDisk, CreateVirtualDisk, GetVirtualDiskPhysicalPath, OpenVirtualDisk,
    ATTACH_VIRTUAL_DISK_FLAG_NONE, ATTACH_VIRTUAL_DISK_PARAMETERS, CREATE_VIRTUAL_DISK_FLAG_NONE,
    CREATE_VIRTUAL_DISK_PARAMETERS, CREATE_VIRTUAL_DISK_VERSION_2, OPEN_VIRTUAL_DISK_FLAG_NONE,
    OPEN_VIRTUAL_DISK_PARAMETERS, VIRTUAL_DISK_ACCESS_ATTACH_RW, VIRTUAL_DISK_ACCESS_GET_INFO,
    VIRTUAL_DISK_ACCESS_NONE, VIRTUAL_STORAGE_TYPE, VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
};

/// VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT — {ec984aec-a0f9-47e9-901f-71415a66345b}.
const VENDOR_MICROSOFT: GUID = GUID {
    data1: 0xec98_4aec,
    data2: 0xa0f9,
    data3: 0x47e9,
    data4: [0x90, 0x1f, 0x71, 0x41, 0x5a, 0x66, 0x34, 0x5b],
};

fn vhdx_storage_type() -> VIRTUAL_STORAGE_TYPE {
    VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VENDOR_MICROSOFT,
    }
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Drive letters D..Z whose root holds a `phoenix-fixture` directory (the marker
/// `fill_fixture` writes). Same trick `winfsp_mount.rs` uses to spot the volume
/// that a freshly attached disk brings online.
fn fixture_letters() -> Vec<char> {
    (b'D'..=b'Z')
        .map(|c| c as char)
        .filter(|l| Path::new(&format!("{l}:\\phoenix-fixture")).exists())
        .collect()
}

/// Materialize our synthesized virtual disk to a real file. The backup came from
/// a 4Kn disk, so `SyntheticVhd` wraps it as a VHDX — which is what we need as a
/// differencing parent. Returns the file's BLAKE3 so we can later prove Windows
/// never wrote to it.
fn materialize_vhdx_parent(backup: &Path, out: &Path) -> anyhow::Result<blake3::Hash> {
    let reader = PhnxReader::open(backup)?;
    let mut vhd = SyntheticVhd::build(reader)?;
    anyhow::ensure!(
        vhd.image_name() == "backup.vhdx",
        "spike needs a VHDX parent, but the backup synthesized a {} — is the source really 4Kn?",
        vhd.image_name()
    );

    let total = vhd.total_len();
    let mut file = std::fs::File::create(out)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut off = 0u64;
    while off < total {
        let n = (buf.len() as u64).min(total - off) as usize;
        vhd.read_at(off, &mut buf[..n])?;
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        off += n as u64;
    }
    file.flush()?;
    Ok(hasher.finalize())
}

/// `CreateVirtualDisk` a differencing VHDX child whose parent is `parent`. Leaving
/// MaximumSize / SectorSize zero makes the child inherit them from the parent —
/// this is exactly how you declare "I am a diff of that disk".
fn create_differencing_vhdx(child: &Path, parent: &Path) -> anyhow::Result<()> {
    let child_w = wide(child);
    let parent_w = wide(parent);
    let storage = vhdx_storage_type();

    // SAFETY: a zeroed Version2 means "dynamic, provider defaults"; we then set
    // only the parent, which is what turns it into a differencing disk. Every
    // field of Version2 is a pointer/int/GUID/POD for which all-zero is valid.
    let mut params: CREATE_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
    params.Version = CREATE_VIRTUAL_DISK_VERSION_2;
    params.Anonymous.Version2.ParentPath = parent_w.as_ptr();
    params.Anonymous.Version2.ParentVirtualStorageType = vhdx_storage_type();

    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: pointers outlive the call; child_w/parent_w are NUL-terminated;
    // optional SD / overlapped are null.
    let err = unsafe {
        CreateVirtualDisk(
            &storage,
            child_w.as_ptr(),
            VIRTUAL_DISK_ACCESS_NONE,
            std::ptr::null_mut(),
            CREATE_VIRTUAL_DISK_FLAG_NONE,
            0,
            &params,
            std::ptr::null(),
            &mut handle,
        )
    };
    if err != 0 {
        anyhow::bail!(
            "CreateVirtualDisk (differencing) rejected our synthesized VHDX as a parent: \
             Win32 error {err} (0x{err:08X}). This is the parent-locator/VirtualDiskID handshake \
             the spike exists to test."
        );
    }
    if !handle.is_null() {
        unsafe { CloseHandle(handle) };
    }
    Ok(())
}

/// A read-WRITE attach of a virtual disk. Dropping detaches it (no
/// PERMANENT_LIFETIME requested), mirroring `phoenix_mount::attach`.
struct AttachedRw {
    handle: HANDLE,
}

impl AttachedRw {
    fn attach(path: &Path) -> anyhow::Result<Self> {
        let w = wide(path);
        let storage = vhdx_storage_type();

        let mut handle: HANDLE = INVALID_HANDLE_VALUE;
        // OPEN_VIRTUAL_DISK_PARAMETERS Version1, RWDepth = 1: open only the top of
        // the chain (the child) writable and the parent(s) read-only — precisely
        // the CoW guarantee we want.
        let mut open: OPEN_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
        open.Version = 1; // OPEN_VIRTUAL_DISK_VERSION_1
        open.Anonymous.Version1.RWDepth = 1;
        let rc = unsafe {
            OpenVirtualDisk(
                &storage,
                w.as_ptr(),
                VIRTUAL_DISK_ACCESS_ATTACH_RW | VIRTUAL_DISK_ACCESS_GET_INFO,
                OPEN_VIRTUAL_DISK_FLAG_NONE,
                &open,
                &mut handle,
            )
        };
        if rc != 0 {
            anyhow::bail!("OpenVirtualDisk (child, RW) failed: Win32 error {rc} (0x{rc:08X})");
        }

        // Read-WRITE attach (no READ_ONLY flag), auto drive letters (no
        // NO_DRIVE_LETTER flag) so the mount manager surfaces the volume.
        let attach = ATTACH_VIRTUAL_DISK_PARAMETERS {
            Version: 1, // ATTACH_VIRTUAL_DISK_VERSION_1
            Anonymous: unsafe { std::mem::zeroed() },
        };
        let rc = unsafe {
            AttachVirtualDisk(
                handle,
                std::ptr::null_mut(),
                ATTACH_VIRTUAL_DISK_FLAG_NONE,
                0,
                &attach,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            unsafe { CloseHandle(handle) };
            anyhow::bail!(
                "AttachVirtualDisk (RW) failed: Win32 error {rc} (0x{rc:08X}). \
                 Mounting requires Administrator."
            );
        }
        Ok(Self { handle })
    }

    /// `N` in `\\.\PhysicalDriveN`, for diagnostics.
    fn physical_drive_number(&self) -> Option<u32> {
        let mut buf = [0u16; 256];
        let mut size_bytes = (buf.len() * 2) as u32;
        let rc =
            unsafe { GetVirtualDiskPhysicalPath(self.handle, &mut size_bytes, buf.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    }
}

impl Drop for AttachedRw {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

#[test]
#[ignore = "requires elevation"]
fn differencing_overlay_over_synthetic_vhdx_parent() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // --- 1. A 4Kn NTFS source with a fixture, backed up to a .phnx ------------
    let source = TestVhd::create_4kn(512).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "OVLSRC".into(),
        }])
        .expect("init 4Kn source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let digest = fill_fixture('X', 0x0FEE_1AB6).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    assert_eq!(disk.sector_size, 4096, "source fixture is not 4Kn");
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();

    let backup_path =
        std::env::temp_dir().join(format!("ovl-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");

    // Detach the source so its identical volume can't be mistaken for the mount.
    drop(source);

    // --- 2. Materialize our synthesized VHDX as a real parent file ------------
    let parent_vhdx =
        std::env::temp_dir().join(format!("ovl-parent-{}.vhdx", uuid::Uuid::new_v4().simple()));
    let parent_hash_before =
        materialize_vhdx_parent(&backup_path, &parent_vhdx).expect("materialize VHDX parent");
    let parent_len = std::fs::metadata(&parent_vhdx).unwrap().len();
    eprintln!(
        "materialized parent: {} ({} bytes), blake3={}",
        parent_vhdx.display(),
        parent_len,
        parent_hash_before.to_hex()
    );

    // --- 3. Differencing child over that parent -------------------------------
    let child_vhdx =
        std::env::temp_dir().join(format!("ovl-child-{}.avhdx", uuid::Uuid::new_v4().simple()));
    create_differencing_vhdx(&child_vhdx, &parent_vhdx)
        .expect("Windows must accept our VHDX as a differencing parent");
    let child_len_fresh = std::fs::metadata(&child_vhdx).unwrap().len();
    eprintln!(
        "created differencing child: {} ({} bytes fresh)",
        child_vhdx.display(),
        child_len_fresh
    );

    // --- 4. Attach the child READ-WRITE and find its volume -------------------
    let pre_existing = fixture_letters();
    let attached = AttachedRw::attach(&child_vhdx).expect("read-write attach of the diff child");
    if let Some(n) = attached.physical_drive_number() {
        eprintln!("child attached as PhysicalDrive{n} (read-write)");
    }

    let mut mounted = None;
    for _ in 0..40 {
        if let Some(l) = fixture_letters()
            .into_iter()
            .find(|l| !pre_existing.contains(l))
        {
            mounted = Some(l);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let letter = mounted.expect(
        "the differencing child attached but no writable NTFS volume came online — \
         Windows may have left the disk offline",
    );
    eprintln!("writable overlay mounted at {letter}:");

    // --- 5a. Existing data reads correctly THROUGH the diff (parent reads) ----
    verify_fixture(letter, &digest).expect("fixture must read back through the differencing child");

    // --- 5b. A brand-new write persists in the CHILD (CoW writes) -------------
    let proof_path = format!("{letter}:\\overlay-proof.bin");
    let proof_bytes: Vec<u8> = (0..64 * 1024).map(|i| (i % 256) as u8).collect();
    {
        let mut f = std::fs::File::create(&proof_path).expect("create file on writable overlay");
        f.write_all(&proof_bytes).expect("write to overlay");
        f.flush().ok();
    }
    let mut read_back = Vec::new();
    std::fs::File::open(&proof_path)
        .expect("reopen overlay file")
        .read_to_end(&mut read_back)
        .expect("read overlay file");
    assert_eq!(
        read_back, proof_bytes,
        "overlay write did not read back intact"
    );
    eprintln!(
        "wrote and read back {} bytes on the writable overlay",
        proof_bytes.len()
    );

    // --- 6. Detach -----------------------------------------------------------
    drop(attached);
    // Give Windows a moment to flush + release the child.
    std::thread::sleep(std::time::Duration::from_secs(1));

    // --- 7. Safety assertions: parent untouched, backup still verifies --------
    let parent_hash_after = {
        let mut hasher = blake3::Hasher::new();
        let mut f = std::fs::File::open(&parent_vhdx).unwrap();
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            let n = f.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        hasher.finalize()
    };
    assert_eq!(
        parent_hash_before, parent_hash_after,
        "PARENT WAS MODIFIED — the overlay is not safe; Windows wrote back to the parent"
    );

    let child_len_after = std::fs::metadata(&child_vhdx).unwrap().len();
    assert!(
        child_len_after > child_len_fresh,
        "differencing child did not grow ({child_len_fresh} -> {child_len_after}); \
         writes did not land in the child"
    );
    eprintln!("child grew {child_len_fresh} -> {child_len_after} bytes (all writes captured here)");

    // The backing .phnx was never opened for writing; prove it still verifies.
    let mut reader = PhnxReader::open(&backup_path).expect("reopen backup");
    reader
        .verify_all(false)
        .expect("backing .phnx must still pass a full verify after the writable mount");
    eprintln!("backing .phnx passes full verify_all — overlay left it pristine");

    // --- cleanup -------------------------------------------------------------
    let _ = std::fs::remove_file(&child_vhdx);
    let _ = std::fs::remove_file(&parent_vhdx);
    let _ = std::fs::remove_file(&backup_path);
}
