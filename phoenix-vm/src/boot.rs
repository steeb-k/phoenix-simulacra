//! Boot orchestration — the productized form of the boot smoke.
//!
//! Ties the pieces together: read the manifest, resolve a [`VmConfig`], open or
//! resume the [`Session`], serve the backup on demand, materialize the write
//! overlay, build the QEMU command line, launch, and tear down. Gated behind
//! `winfsp` because the serve path needs it (and libclang + WinFsp to build).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use phoenix_core::container::PhnxReader;
use phoenix_mount::{create_differencing_vhdx, set_disk_offline, AttachedDisk, WinFspServe};

use crate::config::{BootSpec, DiskSource, VmConfig};
use crate::qemu::Qemu;
use crate::session::{SessionManager, WriteLayer};
use crate::{now_rfc3339, HostOptions};

/// What a boot did.
#[derive(Debug, Clone)]
pub struct BootOutcome {
    pub exit_ok: bool,
    /// Size of the session overlay after teardown — how much the guest wrote.
    pub overlay_bytes: u64,
    /// True if the backing `.phnx` still passes a structural check afterward.
    pub backup_intact: bool,
}

/// Boot `backup` as a VM. Blocks until the guest window is closed (the serve
/// must outlive the guest). The session's overlay + varstore are kept for
/// resume; call [`SessionManager::discard`] to throw the session away.
pub fn boot(
    backup: &Path,
    host: &HostOptions,
    write_layer: WriteLayer,
    qemu: &Qemu,
    sessions: &SessionManager,
    scratch_dir: &Path,
) -> Result<BootOutcome> {
    // --- manifest → config + session identity -------------------------------
    let reader = PhnxReader::open(backup).with_context(|| format!("open {}", backup.display()))?;
    let backup_id = reader.header.backup_id;
    let manifest = reader.manifest.clone();
    drop(reader);

    let cfg = VmConfig::from_manifest(&manifest, host)?;
    let mut session = sessions.open_or_create(backup_id, backup, write_layer, now_rfc3339())?;
    let resuming = session.overlay_exists();

    // --- serve the backup on demand (deterministic path → resume works) -----
    std::fs::create_dir_all(scratch_dir).ok();
    let serve = WinFspServe::serve_for_vm(backup, scratch_dir)
        .context("serve the backup for VM boot")?;
    let parent = serve.image_path();
    let parent_str = parent
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 served parent path"))?;

    // --- firmware (UEFI: per-session writable copies) -----------------------
    let (fw_code, fw_vars) = match cfg.firmware {
        crate::config::Firmware::Uefi => {
            if !qemu.has_uefi_firmware() {
                bail!(
                    "QEMU install at {} is missing the edk2 UEFI firmware needed for this \
                     GPT backup",
                    qemu.share.display()
                );
            }
            session.ensure_uefi_firmware(&qemu.ovmf_code(), &qemu.ovmf_vars_template())?;
            (
                Some(session.firmware_code_path().display().to_string()),
                Some(session.firmware_vars_path().display().to_string()),
            )
        }
        crate::config::Firmware::Bios => (None, None),
    };

    // --- write overlay + disk source ----------------------------------------
    // `_attached` (if any) must outlive the guest: dropping it detaches the disk.
    let overlay = session.overlay_path();
    let (_attached, disk): (Option<AttachedDisk>, DiskSource) = match write_layer {
        WriteLayer::Qcow2 => {
            if !resuming {
                let out = Command::new(&qemu.img)
                    .args(["create", "-f", "qcow2", "-F", "vhdx", "-b"])
                    .arg(&parent)
                    .arg(&overlay)
                    .output()
                    .context("qemu-img create qcow2 overlay")?;
                if !out.status.success() {
                    bail!(
                        "qemu-img create failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
            (
                None,
                DiskSource::File {
                    path: overlay.display().to_string(),
                    format: "qcow2".to_string(),
                },
            )
        }
        WriteLayer::Avhdx => {
            let child_str = overlay
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 overlay path"))?;
            if !resuming {
                // Fresh differencing child over the served parent. On resume we
                // reuse the existing child (it records this same parent path).
                create_differencing_vhdx(child_str, parent_str)
                    .context("create differencing session overlay")?;
            }
            let attached = AttachedDisk::attach_readwrite_opts(child_str, false)
                .context("attach session overlay read-write")?;
            let drive = attached
                .physical_drive_number()
                .context("resolve attached overlay's physical drive number")?;
            set_disk_offline(drive).context("force session overlay disk offline")?;
            (Some(attached), DiskSource::RawPhysicalDrive(drive))
        }
    };

    // --- assemble + launch --------------------------------------------------
    let spec = BootSpec {
        name: format!("Phoenix Simulacra — {}", manifest.hostname),
        firmware_code: fw_code,
        firmware_vars: fw_vars,
        disk,
    };
    let args = cfg.qemu_args(&spec);

    session.mark_booting(now_rfc3339())?;
    tracing::info!(
        "booting {} ({} write layer, {}resume) with {} args",
        backup.display(),
        match write_layer {
            WriteLayer::Avhdx => "avhdx",
            WriteLayer::Qcow2 => "qcow2",
        },
        if resuming { "" } else { "no " },
        args.len()
    );
    let status = Command::new(&qemu.system)
        .args(&args)
        .status()
        .with_context(|| format!("launch {}", qemu.system.display()))?;

    // --- teardown: detach the child, then stop the serve --------------------
    drop(_attached);
    std::thread::sleep(std::time::Duration::from_secs(1));
    drop(serve);
    session.mark_clean()?;

    let overlay_bytes = std::fs::metadata(&overlay).map(|m| m.len()).unwrap_or(0);
    let backup_intact = PhnxReader::open(backup)
        .and_then(|mut r| r.verify_all(true))
        .is_ok();

    Ok(BootOutcome {
        exit_ok: status.success(),
        overlay_bytes,
        backup_intact,
    })
}
