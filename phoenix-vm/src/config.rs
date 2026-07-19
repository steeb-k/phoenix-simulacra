//! Deriving a QEMU VM configuration from a backup's manifest, and turning that
//! configuration into a command line.
//!
//! This is the heart of "zero-question boot": every setting we can infer from
//! what the backup already knows is inferred here, and the whole matrix is a
//! pure function of the manifest plus a few host knobs — so it is unit-tested
//! without QEMU, WinFsp, or elevation.
//!
//! The choices encode the recipe validated live against a real Windows 11
//! backup (see `docs/VIRTUALIZATION.md` "Boot-smoke findings"); the reasons are
//! in the comments so they are not re-learned the hard way.

use anyhow::{bail, Result};
use phoenix_core::manifest::BackupManifest;

/// Guest firmware, selected from the source disk's partitioning style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    /// GPT source → UEFI (OVMF/edk2 pflash pair).
    Uefi,
    /// MBR source → legacy BIOS (SeaBIOS, QEMU's default; no pflash).
    Bios,
}

/// Disk controller presented to the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskController {
    /// AHCI via `ide-hd` on q35. Windows has an inbox `storahci` boot driver,
    /// so a physical install boots without driver injection (validated: a
    /// capture from NVMe hardware booted here with no 0x7B). 512-byte only.
    Ahci,
    /// NVMe (`nvme` controller + `nvme-ns`). Required for 4Kn captures — IDE
    /// and AHCI are 512-byte-sector only — and inbox `stornvme` is boot-start.
    Nvme,
}

/// CPU virtualization backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// Windows Hypervisor Platform, default (in-hypervisor) IRQ chip. Do NOT
    /// pass `kernel-irqchip=off`: with it, Windows wedges at early kernel init
    /// (userspace-APIC timer starvation — no bugcheck, just a hang).
    Whpx,
    /// Pure software emulation. Correct everywhere, slow; the fallback when
    /// WHPX is unavailable (e.g. an ARM64 host running an x86 guest).
    Tcg,
}

impl Accel {
    fn as_arg(self) -> &'static str {
        match self {
            // Default irqchip on purpose — see the enum doc.
            Accel::Whpx => "whpx",
            Accel::Tcg => "tcg",
        }
    }
}

/// The physical thing QEMU opens as the guest's disk. Filled in at boot time
/// once the write overlay exists — the config decides the *controller*, this
/// decides the *backing*.
#[derive(Debug, Clone)]
pub enum DiskSource {
    /// A raw physical device — the Option-A `.avhdx` differencing child,
    /// attached offline with no drive letters (`\\.\PhysicalDrive{0}`).
    RawPhysicalDrive(u32),
    /// A QEMU image file (the Option-B qcow2 overlay), with its format string.
    File { path: String, format: String },
}

/// Host-side knobs that the manifest can't determine.
#[derive(Debug, Clone)]
pub struct HostOptions {
    pub memory_mib: u64,
    pub smp: u32,
    pub accel: Accel,
    /// Force a controller instead of the manifest-derived default. `None` uses
    /// the default (AHCI for 512e, NVMe for 4Kn).
    pub controller_override: Option<DiskController>,
    /// User-mode (slirp) networking with an `e1000e` NIC (inbox driver). When
    /// false, no NIC at all — for forensic boots of possibly-hostile images.
    pub network: bool,
}

impl Default for HostOptions {
    fn default() -> Self {
        HostOptions {
            memory_mib: 6144,
            smp: 4,
            accel: Accel::Whpx,
            controller_override: None,
            network: true,
        }
    }
}

/// A fully-resolved VM configuration: manifest-derived choices plus host knobs.
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub firmware: Firmware,
    pub controller: DiskController,
    pub sector_size: u32,
    /// A rich *named* CPU model — never `max`. Under WHPX on APX-era Intel
    /// hosts, `-cpu max` exposes features the hypervisor can't virtualize and
    /// the guest spins before video init. `Skylake-Client-v4` clears Win11's
    /// CPU floor (SSE4.2/POPCNT) and boots.
    pub cpu_model: String,
    pub accel: Accel,
    pub memory_mib: u64,
    pub smp: u32,
    pub network: bool,
}

impl VmConfig {
    /// Derive a VM configuration from a backup manifest.
    ///
    /// Refuses a backup whose OS volume is a **locked** BitLocker partition:
    /// the image holds raw ciphertext that can't boot without the key, exactly
    /// as the writable mount refuses it.
    pub fn from_manifest(manifest: &BackupManifest, host: &HostOptions) -> Result<Self> {
        if let Some(p) = manifest
            .partitions
            .iter()
            .find(|p| p.bitlocker.as_deref() == Some("locked"))
        {
            bail!(
                "partition {} is a locked BitLocker volume (raw ciphertext) — \
                 it cannot boot without the recovery key",
                p.index
            );
        }

        let firmware = if manifest.disk.style.eq_ignore_ascii_case("gpt") {
            Firmware::Uefi
        } else {
            Firmware::Bios
        };

        let sector_size = match manifest.disk.sector_size {
            4096 => 4096,
            _ => 512,
        };

        // 4Kn forces NVMe (IDE/AHCI are 512-only). Otherwise AHCI: inbox
        // storahci boots Windows without driver injection.
        let controller = host.controller_override.unwrap_or({
            if sector_size == 4096 {
                DiskController::Nvme
            } else {
                DiskController::Ahci
            }
        });

        Ok(VmConfig {
            firmware,
            controller,
            sector_size,
            cpu_model: "Skylake-Client-v4".to_string(),
            accel: host.accel,
            memory_mib: host.memory_mib,
            smp: host.smp,
            network: host.network,
        })
    }

    /// Everything the command line needs that isn't a manifest decision: a VM
    /// name, the resolved firmware files, and the disk backing.
    pub fn qemu_args(&self, spec: &BootSpec) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        let mut push = |s: &str| a.push(s.to_string());

        push("-machine");
        push("q35");
        push("-accel");
        push(self.accel.as_arg());
        if self.accel == Accel::Whpx {
            // Software fallback if WHPX init fails at runtime.
            push("-accel");
            push("tcg");
        }
        push("-cpu");
        push(&self.cpu_model);
        push("-smp");
        push(&self.smp.to_string());
        push("-m");
        push(&format!("{}M", self.memory_mib));
        push("-rtc");
        push("base=localtime");
        push("-name");
        push(&spec.name);

        // QMP control socket for graceful ACPI shutdown (system_powerdown).
        if let Some(port) = spec.qmp_port {
            push("-qmp");
            push(&format!("tcp:127.0.0.1:{port},server,nowait"));
        }

        if self.network {
            push("-nic");
            push("user,model=e1000e");
        } else {
            push("-nic");
            push("none");
        }

        // Firmware: UEFI needs a writable copy of BOTH pflash units. Attaching
        // the code flash read-only maps it as ROM, and WHPX cannot execute from
        // ROM-mapped memory — OVMF then hangs at 100% CPU before video init.
        if self.firmware == Firmware::Uefi {
            if let Some(code) = &spec.firmware_code {
                push("-drive");
                push(&format!("if=pflash,format=raw,file={code}"));
            }
            if let Some(vars) = &spec.firmware_vars {
                push("-drive");
                push(&format!("if=pflash,format=raw,file={vars}"));
            }
        }

        // Boot order. bootindex must be set (a fresh UEFI varstore otherwise
        // burns minutes on PXE/HTTP and drops to the shell). When booting from
        // the ISO this launch, the CD takes priority (0) and the disk follows.
        let booting_iso = spec.iso.is_some() && spec.boot_iso_first;
        let disk_bootindex = if booting_iso { 1 } else { 0 };

        match &spec.disk {
            DiskSource::RawPhysicalDrive(n) => {
                push("-drive");
                push(&format!(r"file=\\.\PhysicalDrive{n},format=raw,if=none,id=disk0"));
            }
            DiskSource::File { path, format } => {
                push("-drive");
                push(&format!("file={path},format={format},if=none,id=disk0"));
            }
        }
        match self.controller {
            DiskController::Ahci => {
                push("-device");
                push(&format!("ide-hd,drive=disk0,bootindex={disk_bootindex}"));
            }
            DiskController::Nvme => {
                push("-device");
                push("nvme,id=nvm0,serial=PHNXVM01");
                // Controller + namespace as separate devices: the legacy
                // `nvme,drive=` shorthand left the controller namespace-less
                // and OVMF found no disk (fell through to PXE).
                let mut ns = format!("nvme-ns,drive=disk0,bus=nvm0,bootindex={disk_bootindex}");
                if self.sector_size == 4096 {
                    ns.push_str(",logical_block_size=4096,physical_block_size=4096");
                }
                push("-device");
                push(&ns);
            }
        }

        // Optional rescue/PE ISO as a read-only CD-ROM. AHCI CD (ide-cd on q35)
        // has inbox drivers in every guest we target.
        if let Some(iso) = &spec.iso {
            push("-drive");
            push(&format!("file={iso},format=raw,if=none,id=cd0,media=cdrom,readonly=on"));
            let cd_bootindex = if booting_iso { 0 } else { 2 };
            push("-device");
            push(&format!("ide-cd,drive=cd0,bootindex={cd_bootindex}"));
        }
        a
    }
}

/// Runtime inputs to the command-line builder that aren't manifest decisions.
#[derive(Debug, Clone)]
pub struct BootSpec {
    pub name: String,
    /// UEFI code flash (a writable per-session copy). `None` for BIOS.
    pub firmware_code: Option<String>,
    /// UEFI NVRAM varstore (per-session, boot order persists here). `None` for BIOS.
    pub firmware_vars: Option<String>,
    pub disk: DiskSource,
    /// An optional read-only CD-ROM ISO (rescue / PE environment).
    pub iso: Option<String>,
    /// Boot from the ISO *this launch* (the "next boot only" affordance). Since
    /// every boot launches a fresh QEMU, "next boot only" is simply a per-launch
    /// flag: set it once, and the *next* launch — without it — boots the disk
    /// normally. When true the CD is given boot priority over the disk.
    pub boot_iso_first: bool,
    /// TCP port for a QMP control socket (`127.0.0.1:PORT`). Enables graceful
    /// ACPI shutdown (`system_powerdown`) instead of killing QEMU. `None` omits
    /// the socket.
    pub qmp_port: Option<u16>,
}

impl BootSpec {
    /// Minimal spec: no ISO, no QMP. Callers set the optional fields.
    pub fn new(name: String, disk: DiskSource) -> Self {
        BootSpec {
            name,
            firmware_code: None,
            firmware_vars: None,
            disk,
            iso: None,
            boot_iso_first: false,
            qmp_port: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::manifest::{BackupManifest, DiskManifest, PartitionManifest};

    fn part(index: u32, fs: &str, bitlocker: Option<&str>) -> PartitionManifest {
        PartitionManifest {
            index,
            name: String::new(),
            type_guid: None,
            fs: fs.to_string(),
            capture_mode: "used".to_string(),
            original_size: 100 * 1024 * 1024,
            used_bytes: 10 * 1024 * 1024,
            bitlocker: bitlocker.map(|s| s.to_string()),
            unique_guid: None,
            gpt_attributes: None,
            chunks: vec![],
            bitmap_hash: None,
        }
    }

    fn manifest(style: &str, sector: u32, parts: Vec<PartitionManifest>) -> BackupManifest {
        BackupManifest {
            format_version: 1,
            backup_id: uuid::Uuid::nil(),
            parent_backup_id: None,
            hostname: "test".to_string(),
            disk: DiskManifest {
                style: style.to_string(),
                disk_guid: None,
                sector_size: sector,
            },
            partitions: parts,
        }
    }

    fn spec_uefi_raw() -> BootSpec {
        BootSpec {
            firmware_code: Some("code.fd".to_string()),
            firmware_vars: Some("vars.fd".to_string()),
            ..BootSpec::new("t".to_string(), DiskSource::RawPhysicalDrive(4))
        }
    }

    #[test]
    fn gpt_512_windows_is_uefi_ahci() {
        let m = manifest("gpt", 512, vec![part(0, "efi", None), part(1, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        assert_eq!(cfg.firmware, Firmware::Uefi);
        assert_eq!(cfg.controller, DiskController::Ahci);
        assert_eq!(cfg.sector_size, 512);
        assert_eq!(cfg.cpu_model, "Skylake-Client-v4");
    }

    #[test]
    fn gpt_4kn_forces_nvme() {
        let m = manifest("gpt", 4096, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        assert_eq!(cfg.controller, DiskController::Nvme);
        assert_eq!(cfg.sector_size, 4096);
    }

    #[test]
    fn mbr_is_bios() {
        let m = manifest("mbr", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        assert_eq!(cfg.firmware, Firmware::Bios);
    }

    #[test]
    fn locked_bitlocker_is_refused() {
        let m = manifest("gpt", 512, vec![part(2, "ntfs", Some("locked"))]);
        let err = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap_err();
        assert!(err.to_string().contains("locked BitLocker"));
    }

    #[test]
    fn unlocked_bitlocker_is_allowed() {
        let m = manifest("gpt", 512, vec![part(2, "ntfs", Some("unlocked"))]);
        assert!(VmConfig::from_manifest(&m, &HostOptions::default()).is_ok());
    }

    #[test]
    fn controller_override_wins() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let host = HostOptions {
            controller_override: Some(DiskController::Nvme),
            ..HostOptions::default()
        };
        let cfg = VmConfig::from_manifest(&m, &host).unwrap();
        assert_eq!(cfg.controller, DiskController::Nvme);
    }

    #[test]
    fn uefi_args_have_two_writable_pflash_and_bootindex() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        // both flashes writable (no readonly=on), and the OS disk pinned first
        assert_eq!(args.matches("if=pflash,format=raw,file=").count(), 2);
        assert!(!args.contains("readonly=on"));
        assert!(args.contains("ide-hd,drive=disk0,bootindex=0"));
        assert!(args.contains(r"file=\\.\PhysicalDrive4,format=raw"));
        // never -cpu max under WHPX
        assert!(args.contains("-cpu Skylake-Client-v4"));
        assert!(!args.contains("-cpu max"));
        // WHPX with the default irqchip, TCG fallback appended
        assert!(args.contains("-accel whpx"));
        assert!(!args.contains("kernel-irqchip=off"));
        assert!(args.contains("-accel tcg"));
    }

    #[test]
    fn bios_args_have_no_pflash() {
        let m = manifest("mbr", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let spec = BootSpec {
            firmware_code: None,
            firmware_vars: None,
            ..spec_uefi_raw()
        };
        let args = cfg.qemu_args(&spec).join(" ");
        assert!(!args.contains("pflash"));
    }

    #[test]
    fn nvme_4kn_args_carry_block_sizes() {
        let m = manifest("gpt", 4096, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        assert!(args.contains("nvme,id=nvm0"));
        assert!(args.contains("nvme-ns,drive=disk0,bus=nvm0,bootindex=0"));
        assert!(args.contains("logical_block_size=4096,physical_block_size=4096"));
    }

    #[test]
    fn qcow2_disk_source_builds_file_drive() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let spec = BootSpec {
            disk: DiskSource::File {
                path: "session.qcow2".to_string(),
                format: "qcow2".to_string(),
            },
            ..spec_uefi_raw()
        };
        let args = cfg.qemu_args(&spec).join(" ");
        assert!(args.contains("file=session.qcow2,format=qcow2,if=none,id=disk0"));
    }

    #[test]
    fn iso_boot_first_gives_cd_priority_over_disk() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let spec = BootSpec {
            iso: Some(r"D:\winpe.iso".to_string()),
            boot_iso_first: true,
            ..spec_uefi_raw()
        };
        let args = cfg.qemu_args(&spec).join(" ");
        assert!(args.contains(r"file=D:\winpe.iso,format=raw,if=none,id=cd0,media=cdrom,readonly=on"));
        // CD boots first, disk second.
        assert!(args.contains("ide-cd,drive=cd0,bootindex=0"));
        assert!(args.contains("ide-hd,drive=disk0,bootindex=1"));
    }

    #[test]
    fn iso_present_but_not_first_leaves_disk_booting() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let spec = BootSpec {
            iso: Some("rescue.iso".to_string()),
            boot_iso_first: false,
            ..spec_uefi_raw()
        };
        let args = cfg.qemu_args(&spec).join(" ");
        assert!(args.contains("ide-hd,drive=disk0,bootindex=0")); // disk still first
        assert!(args.contains("ide-cd,drive=cd0,bootindex=2")); // CD attached, lower priority
    }

    #[test]
    fn no_iso_means_no_cdrom() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        assert!(!args.contains("cdrom"));
        assert!(!args.contains("ide-cd"));
    }

    #[test]
    fn qmp_port_adds_control_socket() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let spec = BootSpec {
            qmp_port: Some(45123),
            ..spec_uefi_raw()
        };
        let args = cfg.qemu_args(&spec).join(" ");
        assert!(args.contains("-qmp tcp:127.0.0.1:45123,server,nowait"));
        // and absent by default
        assert!(!cfg.qemu_args(&spec_uefi_raw()).join(" ").contains("-qmp"));
    }

    #[test]
    fn no_network_uses_nic_none() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let host = HostOptions {
            network: false,
            ..HostOptions::default()
        };
        let cfg = VmConfig::from_manifest(&m, &host).unwrap();
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        assert!(args.contains("-nic none"));
        assert!(!args.contains("e1000e"));
    }
}
