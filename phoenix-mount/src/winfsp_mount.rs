//! Zero-disk-space mount via WinFsp.
//!
//! A WinFsp read-only filesystem exposes a single file, `backup.vhd`, whose
//! bytes are served on demand from the compressed `.phnx` by [`SyntheticVhd`]
//! (no materialization). `AttachVirtualDisk` is then pointed at that file, so
//! Windows' own NTFS/FAT drivers surface the backup's partitions with drive
//! letters — instantly and with no extra disk footprint.
//!
//! This is the shipping mount path (the `image.rs` materialize path is a
//! stopgap). Requires the WinFsp driver installed at run time; the binary
//! delay-loads winfsp-<arch>.dll so it can be bundled with the app.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

use widestring::U16CStr;
use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    WideNameInfo,
};
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
use winfsp::FspError;

use phoenix_core::container::PhnxReader;
use phoenix_core::error::{PhoenixError, Result};

use crate::attach::AttachedDisk;
use crate::synthetic::SyntheticVhd;

/// The single file the volume exposes.
/// The two names the filesystem may serve. Which one depends on the container
/// `SyntheticVhd` chose: a 4Kn backup needs VHDX (only that format can state a
/// sector size), everything else keeps the simpler fixed VHD.
const VHD_NAME: &str = "backup.vhd";
const VHDX_NAME: &str = "backup.vhdx";

// NTSTATUS / attribute constants (avoid pulling extra windows-sys surface).
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_END_OF_FILE: i32 = 0xC000_0011u32 as i32;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Which node an open handle refers to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Node {
    Root,
    Vhd,
}

/// The read-only filesystem serving one synthesized VHD file.
struct VhdFs {
    /// `SyntheticVhd::read_at` needs `&mut` (chunk cache + file reads) and
    /// WinFsp calls us from many dispatcher threads, so guard it.
    vhd: Mutex<SyntheticVhd>,
    total_len: u64,
    /// `backup.vhd` or `backup.vhdx`, per the container.
    image_name: &'static str,
    /// A permissive self-relative security descriptor (grant World read).
    security: Vec<u8>,
    /// Fixed timestamp for the synthesized nodes.
    time: u64,
}

impl VhdFs {
    fn new(vhd: SyntheticVhd) -> Result<Self> {
        let total_len = vhd.total_len();
        let image_name = vhd.image_name();
        Ok(Self {
            vhd: Mutex::new(vhd),
            total_len,
            image_name,
            security: build_security_descriptor(),
            time: now_filetime(),
        })
    }

    fn file_info(&self, node: Node) -> FileInfo {
        let (file_attributes, size, index_number) = match node {
            Node::Root => (FILE_ATTRIBUTE_DIRECTORY, 0, 1),
            Node::Vhd => (FILE_ATTRIBUTE_NORMAL, self.total_len, 2),
        };
        FileInfo {
            creation_time: self.time,
            last_access_time: self.time,
            last_write_time: self.time,
            change_time: self.time,
            file_attributes,
            file_size: size,
            allocation_size: size,
            index_number,
            ..Default::default()
        }
    }

    /// Copy the security descriptor into `buf` if it fits; always return the
    /// size WinFsp needs (it re-calls with a bigger buffer if we say so).
    fn write_security(&self, buf: Option<&mut [c_void]>) -> u64 {
        if let Some(sd) = buf {
            let need = self.security.len();
            if sd.len() >= need {
                let dst =
                    unsafe { std::slice::from_raw_parts_mut(sd.as_mut_ptr() as *mut u8, need) };
                dst.copy_from_slice(&self.security);
            }
        }
        self.security.len() as u64
    }
}

/// Resolve a WinFsp path (`\`, `\backup.vhd`, `\backup.vhdx`) to a node.
///
/// Accepts BOTH names regardless of which this mount advertises. The filesystem
/// hosts exactly one image, so there is nothing to disambiguate — and being
/// liberal here means a stale path can't produce a baffling "file not found"
/// against a mount that is working perfectly.
fn resolve(file_name: &U16CStr) -> Option<Node> {
    let s = file_name.to_string_lossy();
    let norm = s.trim_start_matches('\\');
    if norm.is_empty() {
        Some(Node::Root)
    } else if norm.eq_ignore_ascii_case(VHD_NAME) || norm.eq_ignore_ascii_case(VHDX_NAME) {
        Some(Node::Vhd)
    } else {
        None
    }
}

impl FileSystemContext for VhdFs {
    type FileContext = Node;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let node = resolve(file_name).ok_or(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
        let sz = self.write_security(security_descriptor);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: sz,
            attributes: self.file_info(node).file_attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let node = resolve(file_name).ok_or(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
        *file_info.as_mut() = self.file_info(node);
        Ok(node)
    }

    fn close(&self, _context: Self::FileContext) {}

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        if *context != Node::Vhd {
            return Err(FspError::NTSTATUS(STATUS_END_OF_FILE));
        }
        if offset >= self.total_len {
            return Err(FspError::NTSTATUS(STATUS_END_OF_FILE));
        }
        let end = offset
            .saturating_add(buffer.len() as u64)
            .min(self.total_len);
        let n = (end - offset) as usize;
        let mut vhd = self.vhd.lock().unwrap();
        vhd.read_at(offset, &mut buffer[..n])
            .map_err(|_| FspError::NTSTATUS(STATUS_END_OF_FILE))?;
        Ok(n as u32)
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        *file_info = self.file_info(*context);
        Ok(())
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        out.total_size = self.total_len;
        out.free_size = 0; // read-only, fully used
        out.set_volume_label("Phoenix Simulacra");
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        Ok(self.write_security(security_descriptor))
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if *context != Node::Root {
            return Ok(0);
        }
        let mut cursor = 0u32;
        // Single entry: the VHD file. Emit it only if we haven't already passed
        // it (resume via `marker`).
        let already_past = marker.inner().is_some();
        if !already_past {
            let mut info: DirInfo<255> = DirInfo::new();
            *info.file_info_mut() = self.file_info(Node::Vhd);
            let name: Vec<u16> = self.image_name.encode_utf16().collect();
            info.set_name_raw(name.as_slice())
                .map_err(|_| FspError::NTSTATUS(STATUS_END_OF_FILE))?;
            if !info.append_to_buffer(buffer, &mut cursor) {
                return Ok(cursor);
            }
        }
        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }
}

/// Where WinFsp put the one-file volume that serves the synthesized image.
///
/// A directory under the scratch dir is the default: it keeps this plumbing
/// out of Explorer, so the only drives the user sees are the backup's own
/// partitions (assigned in [`crate::letters`] from the *attached* disk — a
/// separate layer that this choice does not affect).
enum MountLocation {
    /// The hidden scratch directory. WinFsp turns it into a junction.
    Dir(PathBuf),
    /// A spare drive letter, used when the host volume can't hold a junction.
    Drive(char),
}

impl MountLocation {
    /// Directory the image file lives in, whichever form the mount took.
    fn image_dir(&self) -> PathBuf {
        match self {
            Self::Dir(p) => p.clone(),
            Self::Drive(c) => PathBuf::from(format!("{c}:\\")),
        }
    }
}

/// Mount `host`'s volume, preferring `mount_dir` and falling back to a drive
/// letter.
///
/// WinFsp implements a directory mount point as a reparse point on that
/// directory, so a volume whose filesystem has no reparse support rejects it
/// with `STATUS_NOT_IMPLEMENTED` — which is exactly what the WinPE RAM disk
/// (`X:`) does, and PE is where a restore tool has to work. A drive letter
/// needs no reparse point, so spend one rather than fail the mount; the cost
/// is that this internal volume becomes briefly visible.
fn mount_host(
    host: &mut FileSystemHost<VhdFs, FineGuard>,
    mount_dir: PathBuf,
) -> Result<MountLocation> {
    let dir_err = match host.mount(&mount_dir) {
        Ok(()) => return Ok(MountLocation::Dir(mount_dir)),
        Err(e) => e,
    };

    let Some(letter) = crate::letters::free_letters().next() else {
        return Err(PhoenixError::Other(format!(
            "WinFsp mount at {mount_dir:?} failed: {dir_err}, and no drive letter was free to fall back to"
        )));
    };
    tracing::warn!(
        "WinFsp could not mount at {mount_dir:?} ({dir_err}); falling back to drive {letter}: \
         (the volume holding this directory likely has no reparse-point support)"
    );
    host.mount(format!("{letter}:")).map_err(|drive_err| {
        PhoenixError::Other(format!(
            "WinFsp mount failed at {mount_dir:?} ({dir_err}) and at {letter}: ({drive_err})"
        ))
    })?;
    Ok(MountLocation::Drive(letter))
}

/// A live WinFsp-backed mount: the filesystem host plus the attached virtual
/// disk. Dropping detaches the disk, stops the filesystem, and cleans up.
pub struct WinFspMount {
    // Field order matters: `attached` drops first (detach releases the open
    // handle on backup.vhd) before `host` tears down the filesystem.
    attached: Option<AttachedDisk>,
    host: FileSystemHost<VhdFs, FineGuard>,
    mount_point: MountLocation,
    /// `backup.vhd` or `backup.vhdx`, per the container the backup needed.
    image_name: &'static str,
    pub backup_path: PathBuf,
    pub disk_size: u64,
    pub volumes: Vec<crate::letters::MountedVolume>,
    /// Letters we assigned (selection mode); removed again on drop.
    assigned_letters: Vec<char>,
}

impl WinFspMount {
    /// Mount `backup` read-only with zero materialization and all volumes
    /// exposed (Windows assigns the drive letters).
    pub fn mount(backup: &Path, scratch_dir: &Path) -> Result<Self> {
        Self::mount_selected(backup, scratch_dir, None)
    }

    /// Mount `backup` read-only with zero materialization. `scratch_dir` holds
    /// the transient WinFsp mount point (a directory, not a drive letter).
    /// With `selection: Some(indices)` only those partitions get drive
    /// letters; `None` exposes everything (mount-manager policy).
    pub fn mount_selected(
        backup: &Path,
        scratch_dir: &Path,
        selection: Option<&[u32]>,
    ) -> Result<Self> {
        ensure_winfsp()?;
        let reader = PhnxReader::open(backup)?;
        let backup_id = reader.header.backup_id;
        let vhd = SyntheticVhd::build(reader)?;
        let disk_size = vhd.disk_size();
        let spans = vhd.spans().to_vec();
        // Which container the image needs: a 4Kn backup gets a VHDX, because only
        // VHDX can state a sector size. Capture the name before the disk moves
        // into the filesystem — it decides what we hand AttachVirtualDisk.
        let image_name = vhd.image_name();
        let fs = VhdFs::new(vhd)?;

        let mut params = VolumeParams::new();
        params
            .sector_size(512)
            .sectors_per_allocation_unit(1)
            .file_info_timeout(1000)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .persistent_acls(true)
            .read_only_volume(true)
            .filesystem_name("phoenix-mount");

        let mut host = FileSystemHost::<VhdFs, FineGuard>::new(params, fs)
            .map_err(|e| PhoenixError::Other(format!("WinFsp host create failed: {e}")))?;

        std::fs::create_dir_all(scratch_dir)?;
        let mount_dir = scratch_dir.join(format!("mount-{}", backup_id.simple()));
        // WinFsp creates the mount-point directory reparse; make sure a stale
        // one isn't in the way.
        let _ = std::fs::remove_dir_all(&mount_dir);

        let mount_point = mount_host(&mut host, mount_dir)?;
        host.start()
            .map_err(|e| PhoenixError::Other(format!("WinFsp start failed: {e}")))?;

        let vhd_path = mount_point.image_dir().join(image_name);
        let attached = AttachedDisk::attach_readonly_opts(
            vhd_path
                .to_str()
                .ok_or_else(|| PhoenixError::Other("non-UTF-8 mount path".into()))?,
            selection.is_none(),
        )
        .inspect_err(|_| {
            // Tear the FS back down if attach fails, so we don't leak a mount.
            host.stop();
            host.unmount();
        })?;

        let (volumes, assigned_letters) = match selection {
            Some(sel) => {
                let disk = attached.physical_drive_number()?;
                crate::letters::expose_selected(disk, &spans, sel)?
            }
            None => (Vec::new(), Vec::new()),
        };

        Ok(Self {
            attached: Some(attached),
            host,
            mount_point,
            image_name,
            backup_path: backup.to_path_buf(),
            disk_size,
            volumes,
            assigned_letters,
        })
    }

    /// Path to the synthesized image inside the WinFsp mount (the file the
    /// virtual disk is attached from). Useful for diagnostics/tests.
    pub fn vhd_path(&self) -> PathBuf {
        self.mount_point.image_dir().join(self.image_name)
    }

    /// `N` in `\\.\PhysicalDriveN` for the attached synthetic disk.
    ///
    /// Exists so a test can ask **Windows** what it makes of the disk we handed it
    /// — its sector size in particular, which for a 4Kn backup is the entire point
    /// and is not something our own bytes can vouch for.
    pub fn attached_disk_number(&self) -> Result<u32> {
        self.attached
            .as_ref()
            .ok_or_else(|| PhoenixError::Other("virtual disk is not attached".into()))?
            .physical_drive_number()
    }
}

impl Drop for WinFspMount {
    fn drop(&mut self) {
        // Remove any letters we assigned while the volumes still exist…
        crate::letters::remove_letters(&self.assigned_letters);
        // …then detach the virtual disk (releases the open handle on backup.vhd)…
        self.attached.take();
        // …then stop and unmount the filesystem.
        self.host.stop();
        self.host.unmount();
        // Only the directory form leaves anything on disk to clean up; the
        // drive-letter form is gone with `unmount`.
        if let MountLocation::Dir(dir) = &self.mount_point {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// A WinFsp filesystem serving the synthesized image **without** attaching it as
/// a virtual disk — the building block for a writable overlay.
///
/// [`WinFspMount`] serves the image *and* attaches it read-only, so the backup's
/// own volumes surface with drive letters. A writable overlay instead needs the
/// served image to be only the read-only *parent* of a differencing disk, with
/// just the (writable) child attached: attaching the parent too would put a
/// second disk online carrying the same GPT/partition GUIDs as the child, a
/// duplicate Windows keeps offline. So this type brings up the filesystem and
/// hands back the served path, and attaches nothing.
///
/// Dropping stops the filesystem and removes the mount directory.
pub struct WinFspServe {
    host: FileSystemHost<VhdFs, FineGuard>,
    mount_point: MountLocation,
    /// `backup.vhd` or `backup.vhdx`, per the container the backup needed.
    image_name: &'static str,
    pub disk_size: u64,
    /// Partition layout on the synthesized disk, for letter assignment on a
    /// differencing child attached over this parent.
    spans: Vec<crate::chunkstore::PartitionSpan>,
}

impl WinFspServe {
    /// Bring up the WinFsp filesystem serving `backup`'s synthesized image in
    /// `scratch_dir`, without attaching a virtual disk. The returned
    /// [`WinFspServe::image_path`] is the file a differencing disk can name as
    /// its parent.
    ///
    /// `force_vhdx` wraps even a 512e backup in VHDX — required when the served
    /// image is to be a differencing parent, since a fixed VHD cannot be one.
    pub fn serve(backup: &Path, scratch_dir: &Path, force_vhdx: bool) -> Result<Self> {
        Self::serve_inner(backup, scratch_dir, force_vhdx, false)
    }

    /// As [`WinFspServe::serve`] with `force_vhdx`, but the synthesized GPT
    /// keeps the SOURCE disk's identity (disk GUID, partition unique GUIDs,
    /// attribute bits) so a VM can boot it — the BCD resolves its boot device
    /// by those GUIDs. The served image must NOT be attached to the host;
    /// it is meant to back a VM disk (see docs/VIRTUALIZATION.md).
    pub fn serve_for_vm(backup: &Path, scratch_dir: &Path) -> Result<Self> {
        Self::serve_inner(backup, scratch_dir, true, true)
    }

    fn serve_inner(
        backup: &Path,
        scratch_dir: &Path,
        force_vhdx: bool,
        original_identity: bool,
    ) -> Result<Self> {
        ensure_winfsp()?;
        let reader = PhnxReader::open(backup)?;
        let backup_id = reader.header.backup_id;
        let vhd = if original_identity {
            SyntheticVhd::build_vhdx_original_identity(reader)?
        } else if force_vhdx {
            SyntheticVhd::build_vhdx(reader)?
        } else {
            SyntheticVhd::build(reader)?
        };
        let disk_size = vhd.disk_size();
        let image_name = vhd.image_name();
        let spans = vhd.spans().to_vec();
        let fs = VhdFs::new(vhd)?;

        // Identical volume params to WinFspMount: the served file is the same
        // read-only synthesized image; only the attach step differs.
        let mut params = VolumeParams::new();
        params
            .sector_size(512)
            .sectors_per_allocation_unit(1)
            .file_info_timeout(1000)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .persistent_acls(true)
            .read_only_volume(true)
            .filesystem_name("phoenix-mount");

        let mut host = FileSystemHost::<VhdFs, FineGuard>::new(params, fs)
            .map_err(|e| PhoenixError::Other(format!("WinFsp host create failed: {e}")))?;

        std::fs::create_dir_all(scratch_dir)?;
        let mount_dir = scratch_dir.join(format!("serve-{}", backup_id.simple()));
        let _ = std::fs::remove_dir_all(&mount_dir);

        let mount_point = mount_host(&mut host, mount_dir)?;
        host.start()
            .map_err(|e| PhoenixError::Other(format!("WinFsp start failed: {e}")))?;

        Ok(Self {
            host,
            mount_point,
            image_name,
            disk_size,
            spans,
        })
    }

    /// Path to the served image file — the differencing parent.
    pub fn image_path(&self) -> PathBuf {
        self.mount_point.image_dir().join(self.image_name)
    }

    /// Size in bytes of the **synthesized virtual disk** (not the container
    /// file). This is what an attached child or a VM sees as the disk size —
    /// partitions plus the synthesized GPT, which can exceed the source
    /// disk's `size_bytes` from the manifest.
    pub fn disk_size(&self) -> u64 {
        self.disk_size
    }

    /// Partition layout on the synthesized disk (for letter assignment).
    pub fn spans(&self) -> &[crate::chunkstore::PartitionSpan] {
        &self.spans
    }
}

impl Drop for WinFspServe {
    fn drop(&mut self) {
        self.host.stop();
        self.host.unmount();
        if let MountLocation::Dir(dir) = &self.mount_point {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// A **writable overlay** mount: the backup's synthesized image is served
/// read-only through WinFsp as a differencing *parent*, and a differencing
/// *child* (`.avhdx`) on the scratch disk is attached **read-write**. Windows
/// does the copy-on-write — every write lands in the child, the backup is never
/// touched — so the mount is writable while the `.phnx` stays immutable and
/// still verifies, and the footprint grows only with writes.
///
/// The child is **ephemeral**: it is deleted on drop (and swept by
/// [`crate::cleanup_leaked_mounts`] after a crash), so changes are discarded on
/// unmount. That is the guarantee the UI's hazard warning states.
pub struct WinFspWritableMount {
    // Drop order is load-bearing: the RW child must detach (releasing its open
    // handle on the served parent) BEFORE the parent filesystem is torn down.
    attached: Option<AttachedDisk>,
    serve: Option<WinFspServe>,
    /// The differencing child file, deleted on drop.
    child_path: PathBuf,
    pub backup_path: PathBuf,
    pub disk_size: u64,
    pub volumes: Vec<crate::letters::MountedVolume>,
    assigned_letters: Vec<char>,
}

impl WinFspWritableMount {
    /// Mount `backup` WRITABLE with a copy-on-write overlay. `selection` holds
    /// the backup partition indices to expose; `preferred_letters` maps a
    /// partition index to the drive letter it should get if that letter is free
    /// (used to preserve letters when converting a read-only mount in place) —
    /// anything unmatched falls back to first-free.
    pub fn mount_selected(
        backup: &Path,
        scratch_dir: &Path,
        selection: &[u32],
        preferred_letters: &[(u32, char)],
    ) -> Result<Self> {
        ensure_winfsp()?;

        // Refuse locked BitLocker partitions: their image is raw ciphertext, so
        // writing through the overlay would corrupt the volume's own crypto.
        let reader = PhnxReader::open(backup)?;
        let backup_id = reader.header.backup_id;
        for pm in &reader.manifest.partitions {
            if selection.contains(&pm.index) && pm.bitlocker.as_deref() == Some("locked") {
                return Err(PhoenixError::Other(format!(
                    "partition {} is a locked BitLocker volume, so write access is unavailable — \
                     its captured data is raw ciphertext. Unlock BitLocker before backing up to \
                     mount it writable.",
                    pm.index
                )));
            }
        }
        drop(reader);

        // Serve the backup as a read-only differencing PARENT (forced VHDX, since
        // a fixed VHD cannot be a differencing parent).
        let serve = WinFspServe::serve(backup, scratch_dir, true)?;
        let parent_path = serve.image_path();
        let disk_size = serve.disk_size;
        let spans = serve.spans().to_vec();

        // Create the ephemeral differencing CHILD on the scratch disk. Keyed by
        // backup_id like the serve-/mount- junctions, so crash cleanup can find
        // all of a mount's artifacts together.
        let child_path = scratch_dir.join(format!("child-{}.avhdx", backup_id.simple()));
        let _ = std::fs::remove_file(&child_path);
        let parent_str = parent_path
            .to_str()
            .ok_or_else(|| PhoenixError::Other("non-UTF-8 served parent path".into()))?;
        let child_str = child_path
            .to_str()
            .ok_or_else(|| PhoenixError::Other("non-UTF-8 child path".into()))?;
        crate::attach::create_differencing_vhdx(child_str, parent_str)?;

        // Attach the child READ-WRITE with no drive letters, then expose only the
        // selected partitions (reusing prior letters where possible). On any
        // failure past this point, delete the orphaned child.
        let attached = AttachedDisk::attach_readwrite_opts(child_str, false).inspect_err(|_| {
            let _ = std::fs::remove_file(&child_path);
        })?;
        let disk = match attached.physical_drive_number() {
            Ok(d) => d,
            Err(e) => {
                drop(attached);
                let _ = std::fs::remove_file(&child_path);
                return Err(e);
            }
        };
        let (volumes, assigned_letters) = crate::letters::expose_selected_with_letters(
            disk,
            &spans,
            selection,
            preferred_letters,
        )?;

        Ok(Self {
            attached: Some(attached),
            serve: Some(serve),
            child_path,
            backup_path: backup.to_path_buf(),
            disk_size,
            volumes,
            assigned_letters,
        })
    }
}

impl Drop for WinFspWritableMount {
    fn drop(&mut self) {
        // Remove assigned letters while the volumes still exist…
        crate::letters::remove_letters(&self.assigned_letters);
        // …detach the RW child (releases its handle on the served parent)…
        self.attached.take();
        // …tear down the parent filesystem…
        self.serve.take();
        // …then delete the ephemeral diff so writes never persist.
        let _ = std::fs::remove_file(&self.child_path);
    }
}

/// Initialize WinFsp once per process. `winfsp_init` loads winfsp-<arch>.dll
/// from the WinFsp install directory (via the registry) so the delay-loaded
/// symbols resolve; it must run before any other WinFsp call. The token it
/// returns is a `Copy` marker with no `Drop`, so dropping it tears nothing
/// down and WinFsp stays initialized for the process lifetime on its own.
fn ensure_winfsp() -> Result<()> {
    static INIT: Once = Once::new();
    static OK: AtomicBool = AtomicBool::new(false);
    static ERR: Mutex<Option<String>> = Mutex::new(None);
    INIT.call_once(|| match winfsp::winfsp_init() {
        Ok(_token) => {
            OK.store(true, Ordering::SeqCst);
        }
        Err(e) => {
            *ERR.lock().unwrap() = Some(format!("{e:?}"));
        }
    });
    if OK.load(Ordering::SeqCst) {
        Ok(())
    } else {
        let detail = ERR.lock().unwrap().clone().unwrap_or_default();
        Err(PhoenixError::Other(format!(
            "WinFsp initialization failed ({detail}) — is WinFsp installed? (https://winfsp.dev)"
        )))
    }
}

/// Whether WinFsp is present and usable for mounting. Returns true when the
/// user-mode DLL initializes: `winfsp_init` (the `system` feature) loads
/// winfsp-<arch>.dll from the registry-recorded InstallDir, so success means
/// WinFsp is installed and its runtime is reachable — the prerequisite for a
/// mount to attach. A missing or broken install fails here, which is the case
/// the GUI gate exists to catch (portable runs, locked-down machines).
///
/// The kernel driver is registered under a per-install, timestamped service
/// name (e.g. `WinFsp+20260708T041027Z`), so there is no fixed service to
/// probe for "driver loaded"; the DLL init is the reliable signal, and any
/// remaining driver-load failure surfaces as a clear error at mount time.
///
/// Cheap to call repeatedly: init runs at most once (guarded by `Once`) and
/// the cached outcome is returned thereafter.
pub fn is_available() -> bool {
    ensure_winfsp().is_ok()
}

/// Build a permissive self-relative security descriptor granting World read
/// (`O:BA G:BA D:P(A;;FA;;;WD)`), returned as raw bytes. Empty on failure.
fn build_security_descriptor() -> Vec<u8> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

    let sddl: Vec<u16> = "O:BAG:BAD:P(A;;FA;;;WD)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut psd: *mut c_void = null_mut();
    let mut len: u32 = 0;
    unsafe {
        let ok = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1, // SDDL_REVISION_1
            &mut psd,
            &mut len,
        );
        if ok == 0 || psd.is_null() {
            return Vec::new();
        }
        let bytes = std::slice::from_raw_parts(psd as *const u8, len as usize).to_vec();
        LocalFree(psd);
        bytes
    }
}

/// Current time as a Windows FILETIME (100 ns ticks since 1601).
fn now_filetime() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    let mut ft = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    unsafe { GetSystemTimeAsFileTime(&mut ft) };
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}
