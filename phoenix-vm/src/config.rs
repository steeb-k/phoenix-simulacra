//! Deriving a QEMU VM configuration from a backup's manifest, and turning that
//! configuration into a command line.
//!
//! This is the heart of "zero-question boot": every setting we can infer from
//! what the backup already knows is inferred here, and the whole matrix is a
//! pure function of the manifest plus a few host knobs — so it is unit-tested
//! without QEMU, WinFsp, or elevation.
//!
//! The choices encode the recipe validated live against a real Windows 11
//! backup (see `docs/VIRTUALIZATION.md` "The QEMU recipe"); the reasons are
//! in the comments so they are not re-learned the hard way.

use anyhow::{bail, Result};
use phoenix_core::manifest::BackupManifest;

/// Id of the user-mode network backend.
pub const NET_BACKEND_ID: &str = "net0";
/// Id of the NIC device. Named explicitly so `set_link` can plug and unplug
/// its cable while the VM runs — the `-nic` shorthand generates an id that
/// cannot be referred to reliably afterwards.
pub const NET_DEVICE_ID: &str = "nic0";

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
    /// Start with the guest's network cable plugged in.
    ///
    /// Defaults OFF, and the GUI never turns it on at boot: a connected
    /// guest gets a display driver pushed by Windows Update within moments
    /// of first login — well before any in-guest script can block it — and
    /// that driver blanks the screen on the next reboot. Networking is
    /// therefore opt-in, granted after boot with
    /// [`qmp::set_link`](crate::qmp::set_link).
    ///
    /// The NIC itself is always present regardless; this is link state, not
    /// hardware. Link state is host-side and the guest cannot raise its own
    /// cable, so an unplugged NIC isolates the guest just as an absent one
    /// would — while leaving something to plug in later.
    pub network: bool,
    /// Advisory usable-screen size ([`usable_guest_resolution`]). No longer
    /// steers the video device — virtio-vga's GOP tops out at 1920x1080 and
    /// zoom-to-fit scales the picture to the (maximized) window — but kept
    /// for future use as a native-mode hint.
    pub max_resolution: Option<(u32, u32)>,
    // `grab_on_hover` lived here, driving `-display gtk,grab-on-hover=on` so
    // guest hotkeys reached the guest. Removed after testing: it did not
    // capture reliably, and while enabled it broke `Ctrl+Alt+G` — the manual
    // grab toggle that works fine on its own. Use that instead.
    /// Dress the QEMU window's GTK chrome — menu bar, borders, the padding
    /// around the guest picture — in a dark theme, to match the app.
    ///
    /// QEMU has no flag for this; GTK reads it from the environment, so it
    /// is set on the QEMU child process only and never leaks to other GTK
    /// apps on the machine. Affects the chrome alone: the guest's own
    /// picture is whatever the guest paints.
    pub dark_window: bool,
    /// Host↔guest clipboard: both halves of it, since neither works alone —
    /// `clipboard=on` on the GTK display (the host-side peer) and the
    /// qemu-vdagent channel the guest's SPICE vdagent talks to.
    ///
    /// Requires a QEMU whose GTK display accepts `clipboard=on`
    /// ([`Qemu::gtk_clipboard`](crate::Qemu::gtk_clipboard) probes it);
    /// callers should set this from that probe. Off by default so a build
    /// without the option never gets an argument it would reject.
    ///
    /// This was previously suspected of causing the black screens, and it
    /// isn't: those came from the display driver Windows Update delivers.
    pub clipboard_agent: bool,
}

impl HostOptions {
    /// Extra environment for the QEMU child process.
    ///
    /// GTK picks its theme up from the environment, which is the only lever
    /// QEMU exposes here — there is no `-display gtk,dark=on`. Adwaita and
    /// its dark variant are compiled into GTK3, so this needs no theme
    /// files installed. Empty when not wanted, so QEMU just inherits ours.
    pub fn gtk_env(&self) -> Vec<(&'static str, &'static str)> {
        if self.dark_window {
            vec![("GTK_THEME", "Adwaita:dark")]
        } else {
            Vec::new()
        }
    }
}

impl Default for HostOptions {
    fn default() -> Self {
        HostOptions {
            memory_mib: 6144,
            smp: 4,
            accel: Accel::Whpx,
            controller_override: None,
            network: false,
            dark_window: false,
            max_resolution: None,
            clipboard_agent: false,
        }
    }
}

/// Currently *available* (free) physical RAM in MiB — for advisory UI like
/// coloring the memory slider. `None` if the query fails.
pub fn host_free_mem_mib() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: `st` is a zeroed, properly-sized MEMORYSTATUSEX with dwLength
    // set; a failed call returns 0 and the struct is not read.
    unsafe {
        let mut st: MEMORYSTATUSEX = std::mem::zeroed();
        st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        (GlobalMemoryStatusEx(&mut st) != 0).then(|| st.ullAvailPhys / (1024 * 1024))
    }
}

/// The largest guest resolution whose QEMU window still fits on the host's
/// primary display: the work area (screen minus taskbar) less the window's own
/// chrome (title bar, resize frame, and QEMU's menu bar). `None` if the query
/// fails (headless session).
pub fn usable_guest_resolution() -> Option<(u32, u32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SystemParametersInfoW, SM_CXPADDEDBORDER, SM_CXSIZEFRAME,
        SM_CYCAPTION, SM_CYSIZEFRAME, SPI_GETWORKAREA,
    };

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: rect is a properly-sized writable RECT; a failed call returns 0
    // and rect is not read.
    let ok = unsafe {
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut rect as *mut RECT as *mut _, 0)
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: GetSystemMetrics takes no pointers.
    let (frame_x, frame_y, caption, padded) = unsafe {
        (
            GetSystemMetrics(SM_CXSIZEFRAME),
            GetSystemMetrics(SM_CYSIZEFRAME),
            GetSystemMetrics(SM_CYCAPTION),
            GetSystemMetrics(SM_CXPADDEDBORDER),
        )
    };
    // QEMU's GTK display puts its own menu bar under the title bar; there is
    // no system metric for another process's widgets, so allow a fixed band.
    const QEMU_MENUBAR: i32 = 32;
    let w = (rect.right - rect.left) - 2 * (frame_x + padded);
    let h = (rect.bottom - rect.top) - caption - QEMU_MENUBAR - 2 * (frame_y + padded);
    // Round down to multiples of 8 — tidy video-mode numbers.
    (w > 0 && h > 0).then(|| ((w as u32) & !7, (h as u32) & !7))
}

/// Host ceilings for the memory / vCPU knobs: (total physical RAM in MiB,
/// logical CPU count). Falls back to 16 GiB / 8 if the OS queries fail, so a
/// slider built from this is never empty.
pub fn host_caps() -> (u64, u32) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: `st` is a zeroed, properly-sized MEMORYSTATUSEX with dwLength
    // set, as GlobalMemoryStatusEx requires; a failed call returns 0 and the
    // struct is not read.
    let mem_mib = unsafe {
        let mut st: MEMORYSTATUSEX = std::mem::zeroed();
        st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut st) != 0 {
            st.ullTotalPhys / (1024 * 1024)
        } else {
            16 * 1024
        }
    };
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(8);
    (mem_mib, cpus)
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
    /// See [`HostOptions::max_resolution`].
    pub max_resolution: Option<(u32, u32)>,
    /// See [`HostOptions::clipboard_agent`].
    pub clipboard_agent: bool,
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
            max_resolution: host.max_resolution,
            clipboard_agent: host.clipboard_agent,
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

        // Explicit GTK display: zoom-to-fit scales the guest to the window
        // so the maximized window (boot.rs maximizes it once it appears) is
        // actually filled instead of letterboxed. `clipboard=on` is the
        // host-side half of clipboard sharing and only exists on QEMU 11.1+
        // — older builds reject the key outright, hence the gate.
        let mut display = String::from("gtk,zoom-to-fit=on");
        if self.clipboard_agent {
            display.push_str(",clipboard=on");
        }
        push("-display");
        push(&display);

        // QMP control socket for graceful ACPI shutdown (system_powerdown).
        if let Some(port) = spec.qmp_port {
            push("-qmp");
            push(&format!("tcp:127.0.0.1:{port},server,nowait"));
        }

        // The NIC always exists; what changes is whether its cable is
        // plugged in (see [`HostOptions::network`]). Spelled out as a
        // netdev plus a device rather than the shorthand `-nic` because
        // only this form lets us name the device — `set_link` needs that
        // name to unplug or plug the cable while the VM runs, and `-nic`
        // generates an id we cannot reliably refer to afterwards.
        push("-netdev");
        push(&format!("user,id={NET_BACKEND_ID}"));
        push("-device");
        push(&format!(
            "e1000e,netdev={NET_BACKEND_ID},id={NET_DEVICE_ID}"
        ));

        // One virtio-serial controller carries both optional channels below.
        if self.clipboard_agent || spec.qga_port.is_some() {
            push("-device");
            push("virtio-serial-pci");
        }

        // Guest-side half of the clipboard: qemu-vdagent implements enough
        // of the SPICE agent protocol in-process for the guest's vdagent
        // service (from the CD's guest-tools bundler — NOT the loose
        // driver MSI) to talk to, with no SPICE server or client involved.
        if self.clipboard_agent {
            push("-chardev");
            push("qemu-vdagent,id=vdagent0,name=vdagent,clipboard=on");
            push("-device");
            push("virtserialport,chardev=vdagent0,name=com.redhat.spice.0");
        }

        // QEMU guest agent channel: lets the host run diagnostics/repairs
        // inside a guest that has qemu-ga (from the VMSCRIPTS driver script).
        if let Some(port) = spec.qga_port {
            push("-chardev");
            push(&format!(
                "socket,id=qga0,host=127.0.0.1,port={port},server=on,wait=off"
            ));
            push("-device");
            push("virtserialport,chardev=qga0,name=org.qemu.guest_agent.0");
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

        // Video: virtio-vga, always. Two reasons over std VGA:
        // - The virtio-win guest tools install display drivers, and recent
        //   QXL-DOD builds CLAIM QEMU's stdvga and render black at WDDM
        //   handoff (observed: guest framebuffer black at the DOD-default
        //   1280x800 after installing tools). virtio-vga is driven by the
        //   maintained viogpu driver instead — and boots fine driverless too
        //   (Basic Display over the GOP framebuffer; PE validated).
        // - Its firmware GOP tops out at 1920x1080 (measured), and with
        //   zoom-to-fit + the maximized window the picture scales to the
        //   screen, which retires the old VGA vgamem-capping hack entirely.
        // The device must be STABLE across a session's boots (a swap makes
        // Windows re-detect displays), so this is unconditional, not
        // per-boot-kind.
        push("-vga");
        push("none");
        push("-device");
        push("virtio-vga");

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
        // has inbox drivers in every guest we target. The CD gets its OWN SATA
        // port (`bus=ide.1`): q35's AHCI ports are single-unit, and without an
        // explicit bus QEMU auto-places the CD as unit 1 of the port the OS
        // disk already owns and refuses to start ("bus supports only 1 units").
        // Port 1 is always free — the disk is ide.0 (AHCI) or on NVMe.
        if let Some(iso) = &spec.iso {
            push("-drive");
            push(&format!("file={iso},format=raw,if=none,id=cd0,media=cdrom,readonly=on"));
            let cd_bootindex = if booting_iso { 0 } else { 2 };
            push("-device");
            push(&format!("ide-cd,drive=cd0,bus=ide.1,bootindex={cd_bootindex}"));
        }

        // Guest-tools / driver ISO (virtio-win) as a second CD on its own SATA
        // port. Deliberately no bootindex: it exists to be browsed from inside
        // the guest, never booted.
        if let Some(iso) = &spec.drivers_iso {
            push("-drive");
            push(&format!("file={iso},format=raw,if=none,id=cd1,media=cdrom,readonly=on"));
            push("-device");
            push("ide-cd,drive=cd1,bus=ide.2");
        }

        // The "VMSCRIPTS" helper disk (MapShare.cmd) on the next SATA port.
        // A plain raw disk, no bootindex — the guest just opens it.
        if let Some(img) = &spec.helper_disk {
            push("-drive");
            push(&format!("file={img},format=raw,if=none,id=helper0"));
            push("-device");
            push("ide-hd,drive=helper0,bus=ide.3");
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
    /// TCP port for the QEMU guest-agent channel (`org.qemu.guest_agent.0`
    /// virtserialport → loopback socket). Inert until the guest has the
    /// vioserial driver + qemu-ga service (both installed by the VMSCRIPTS
    /// disk's driver script); with them, the host can run diagnostics inside
    /// the guest — the lever that cracked the black-screen investigation.
    pub qga_port: Option<u16>,
    /// The guest-tools / driver ISO (virtio-win), attached as a second
    /// read-only CD-ROM. Never given boot priority — the guest browses it to
    /// install drivers and helpers.
    pub drivers_iso: Option<String>,
    /// Path to the per-session "VMSCRIPTS" helper disk image (FAT, holds
    /// MapShare.cmd; see `share::build_helper_disk`), attached as an extra
    /// never-booted disk.
    pub helper_disk: Option<String>,
}

impl BootSpec {
    /// Minimal spec: no ISOs, no QMP. Callers set the optional fields.
    pub fn new(name: String, disk: DiskSource) -> Self {
        BootSpec {
            name,
            firmware_code: None,
            firmware_vars: None,
            disk,
            iso: None,
            boot_iso_first: false,
            qmp_port: None,
            qga_port: None,
            drivers_iso: None,
            helper_disk: None,
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
        // CD boots first (on its own SATA port), disk second.
        assert!(args.contains("ide-cd,drive=cd0,bus=ide.1,bootindex=0"));
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
        assert!(args.contains("ide-cd,drive=cd0,bus=ide.1,bootindex=2")); // CD attached, lower priority
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
    fn clipboard_agent_gated_off_by_default() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        // Default off: a pre-11.1 QEMU rejects `clipboard=on` outright, so
        // neither half may appear unless the caller probed for it.
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        assert!(!args.contains("vdagent"));
        assert!(!args.contains("virtio-serial-pci"));
        assert!(args.contains("-display gtk,zoom-to-fit=on"));
        assert!(!args.contains("clipboard=on"));

        // Opted in: BOTH halves appear — host-side peer on the display and
        // the guest-facing vdagent channel. Neither is any use alone.
        let host = HostOptions {
            clipboard_agent: true,
            ..HostOptions::default()
        };
        let cfg = VmConfig::from_manifest(&m, &host).unwrap();
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        assert!(args.contains("-display gtk,zoom-to-fit=on,clipboard=on"));
        assert!(args.contains("virtio-serial-pci"));
        assert!(args.contains("qemu-vdagent,id=vdagent0,name=vdagent,clipboard=on"));
        assert!(args.contains("virtserialport,chardev=vdagent0,name=com.redhat.spice.0"));
    }

    #[test]
    fn grab_on_hover_never_comes_back() {
        // It captured unreliably AND broke Ctrl+Alt+G, the manual toggle
        // that works on its own. Nothing may put it back on the display arg.
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let host = HostOptions {
            clipboard_agent: true,
            ..HostOptions::default()
        };
        let cfg = VmConfig::from_manifest(&m, &host).unwrap();
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        assert!(!args.contains("grab-on-hover"));
        assert!(args.contains("-display gtk,zoom-to-fit=on,clipboard=on"));
    }

    #[test]
    fn dark_window_only_sets_the_gtk_theme_env() {
        // QEMU has no dark-mode flag; GTK reads the environment. Set on the
        // child only, so it can never affect other GTK apps on the machine.
        assert!(HostOptions::default().gtk_env().is_empty());
        let host = HostOptions {
            dark_window: true,
            ..HostOptions::default()
        };
        assert_eq!(host.gtk_env(), vec![("GTK_THEME", "Adwaita:dark")]);
        // It is environment only — never an argument.
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &host).unwrap();
        assert!(!cfg.qemu_args(&spec_uefi_raw()).join(" ").contains("GTK_THEME"));
    }

    #[test]
    fn nic_is_always_present_and_nameable() {
        // Networking is opt-in (Windows Update pushes a screen-blanking
        // display driver to a connected guest before any in-guest script can
        // block it), but the NIC must exist even when the cable starts
        // unplugged — otherwise there is nothing for set_link to plug in.
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        for network in [false, true] {
            let host = HostOptions {
                network,
                ..HostOptions::default()
            };
            let cfg = VmConfig::from_manifest(&m, &host).unwrap();
            let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
            assert!(args.contains("-netdev user,id=net0"));
            // Explicit ids, not the `-nic` shorthand: set_link addresses the
            // device by name, and a generated id cannot be relied on.
            assert!(args.contains("-device e1000e,netdev=net0,id=nic0"));
            assert!(!args.contains("-nic"));
        }
    }

    #[test]
    fn helper_disk_rides_port_three_and_never_boots() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let spec = BootSpec {
            helper_disk: Some(r"D:\PhoenixSimulacra\share-helper.img".to_string()),
            ..spec_uefi_raw()
        };
        let args = cfg.qemu_args(&spec).join(" ");
        assert!(args
            .contains(r"file=D:\PhoenixSimulacra\share-helper.img,format=raw,if=none,id=helper0"));
        assert!(args.contains("ide-hd,drive=helper0,bus=ide.3"));
        assert!(!args.contains("helper0,bus=ide.3,bootindex"));
        // Absent when not sharing.
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        assert!(!args.contains("helper0"));
    }

    #[test]
    fn drivers_iso_rides_its_own_port_and_never_boots() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let spec = BootSpec {
            iso: Some(r"D:\winpe.iso".to_string()),
            boot_iso_first: true,
            drivers_iso: Some(r"C:\app\virtio-win.iso".to_string()),
            ..spec_uefi_raw()
        };
        let args = cfg.qemu_args(&spec).join(" ");
        assert!(args.contains(r"file=C:\app\virtio-win.iso,format=raw,if=none,id=cd1,media=cdrom,readonly=on"));
        assert!(args.contains("ide-cd,drive=cd1,bus=ide.2"));
        // The drivers CD carries no bootindex — only the rescue CD and disk do.
        assert!(!args.contains("cd1,bus=ide.2,bootindex"));
        // And it coexists with the rescue CD on its separate port.
        assert!(args.contains("ide-cd,drive=cd0,bus=ide.1,bootindex=0"));
    }

    #[test]
    fn video_is_virtio_vga_for_every_boot_kind() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        // Disk boot, ISO boot, ISO merely attached: the device never changes
        // (a swap makes Windows re-detect displays mid-session).
        for spec in [
            spec_uefi_raw(),
            BootSpec {
                iso: Some(r"D:\winpe.iso".to_string()),
                boot_iso_first: true,
                ..spec_uefi_raw()
            },
            BootSpec {
                iso: Some(r"D:\winpe.iso".to_string()),
                boot_iso_first: false,
                ..spec_uefi_raw()
            },
        ] {
            let args = cfg.qemu_args(&spec).join(" ");
            assert!(args.contains("-vga none"));
            assert!(args.contains("-device virtio-vga"));
            assert!(!args.contains("vgamem"));
        }
    }

    #[test]
    fn qga_port_adds_guest_agent_channel() {
        let m = manifest("gpt", 512, vec![part(0, "ntfs", None)]);
        let cfg = VmConfig::from_manifest(&m, &HostOptions::default()).unwrap();
        let spec = BootSpec {
            qga_port: Some(45222),
            ..spec_uefi_raw()
        };
        let args = cfg.qemu_args(&spec).join(" ");
        assert!(args.contains("virtio-serial-pci"));
        assert!(args.contains("socket,id=qga0,host=127.0.0.1,port=45222,server=on,wait=off"));
        assert!(args.contains("virtserialport,chardev=qga0,name=org.qemu.guest_agent.0"));
        // No channel requested → no serial controller at all.
        let args = cfg.qemu_args(&spec_uefi_raw()).join(" ");
        assert!(!args.contains("virtio-serial-pci"));
        assert!(!args.contains("qga0"));
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

}
