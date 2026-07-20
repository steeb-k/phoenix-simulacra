//! Whether hardware acceleration (WHPX) is actually available, and if not,
//! which of the two very different reasons applies.
//!
//! QEMU is launched with `-accel whpx -accel tcg`, an ordered fallback list, so
//! a machine without WHPX still boots — just in software emulation, which for a
//! Windows guest is the difference between minutes and hours. That makes the
//! silent fallback worse than useless on its own: the user waits, concludes the
//! feature is broken, and never learns it is one checkbox away from working.
//!
//! So probe up front and say something specific. The two failures need
//! completely different actions from the user:
//!
//! - **Virtualization disabled in firmware** — a reboot into BIOS/UEFI.
//! - **Windows Hypervisor Platform not enabled** — a Windows Features checkbox.
//!
//! "Enable virtualization" as a single message would send half of them to the
//! wrong place.

/// Why hardware acceleration is unavailable, when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelBlocker {
    /// The CPU supports virtualization but firmware has it switched off.
    /// Fixed in BIOS/UEFI setup, not in Windows.
    FirmwareDisabled,
    /// Firmware is fine, but the Windows Hypervisor Platform feature — the API
    /// QEMU's WHPX accelerator talks to — is not installed. A Windows Features
    /// checkbox plus a reboot.
    PlatformFeatureMissing,
    /// The platform API is present and firmware is enabled, yet no hypervisor
    /// is running. Seen when Hyper-V's own hypervisor is disabled
    /// (`hypervisorlaunchtype off`) or another hypervisor holds the CPU.
    HypervisorNotRunning,
}

impl AccelBlocker {
    /// A short headline for the UI.
    pub fn headline(self) -> &'static str {
        match self {
            AccelBlocker::FirmwareDisabled => "Virtualization is turned off in this PC's firmware",
            AccelBlocker::PlatformFeatureMissing => "Windows Hypervisor Platform isn't enabled",
            AccelBlocker::HypervisorNotRunning => "The Windows hypervisor isn't running",
        }
    }

    /// Label for a button that applies the fix from here, when the fix is
    /// something Windows can do. Firmware has none — that one is a reboot into
    /// BIOS setup and no amount of elevation reaches it.
    pub fn fix_label(self) -> Option<&'static str> {
        match self {
            AccelBlocker::FirmwareDisabled => None,
            AccelBlocker::PlatformFeatureMissing => Some("Enable Windows Hypervisor Platform"),
            AccelBlocker::HypervisorNotRunning => Some("Start the Windows hypervisor"),
        }
    }

    /// Apply the fix. Blocking and slow (DISM takes tens of seconds), so call
    /// it off the UI thread. Requires elevation, which the app already has.
    ///
    /// Both changes need a restart to take effect, so a success here means
    /// "queued", not "fixed" — say so to the user rather than leaving them to
    /// wonder why nothing changed.
    pub fn apply_fix(self) -> Result<(), String> {
        let (program, args): (&str, &[&str]) = match self {
            AccelBlocker::FirmwareDisabled => {
                return Err("This has to be changed in the PC's firmware setup.".into())
            }
            // DISM rather than PowerShell's Enable-WindowsOptionalFeature:
            // same effect, but it is a plain executable with a stable exit
            // code, where the cmdlet needs a module that isn't present on
            // every SKU and reports failure as a formatted object.
            AccelBlocker::PlatformFeatureMissing => (
                "dism.exe",
                &[
                    "/online",
                    "/enable-feature",
                    "/featurename:HypervisorPlatform",
                    "/all",
                    "/norestart",
                ],
            ),
            AccelBlocker::HypervisorNotRunning => {
                ("bcdedit.exe", &["/set", "hypervisorlaunchtype", "auto"])
            }
        };
        run_hidden(program, args)
    }

    /// What the user should actually do, phrased as an instruction. Kept
    /// concrete: each of these is a different place, and a vague "enable
    /// virtualization" sends people to the wrong one.
    pub fn remedy(self) -> &'static str {
        match self {
            AccelBlocker::FirmwareDisabled => {
                "Restart, enter your BIOS/UEFI setup, and enable virtualization — it is usually \
                 called Intel VT-x, AMD-V or SVM Mode, often under CPU or Advanced settings."
            }
            AccelBlocker::PlatformFeatureMissing => {
                "Open \"Turn Windows features on or off\", tick Windows Hypervisor Platform, \
                 and restart."
            }
            AccelBlocker::HypervisorNotRunning => {
                "Run this in an elevated Command Prompt and restart:  \
                 bcdedit /set hypervisorlaunchtype auto"
            }
        }
    }
}

/// Run a console tool without flashing a window, mapping a non-zero exit to
/// its own output — DISM and bcdedit both explain themselves on failure.
#[cfg(windows)]
fn run_hidden(program: &str, args: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // DISM writes its errors to stdout, bcdedit to stderr.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_string();
    Err(format!("{program} failed: {detail}"))
}

/// Result of probing the host for hardware acceleration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelStatus {
    /// WHPX is usable; guests run at hardware speed.
    Whpx,
    /// WHPX is not usable. QEMU will fall back to TCG software emulation,
    /// which works but is drastically slower for a Windows guest.
    SoftwareOnly(AccelBlocker),
}

impl AccelStatus {
    pub fn is_accelerated(self) -> bool {
        matches!(self, AccelStatus::Whpx)
    }

    /// The blocker, when acceleration is unavailable.
    pub fn blocker(self) -> Option<AccelBlocker> {
        match self {
            AccelStatus::Whpx => None,
            AccelStatus::SoftwareOnly(b) => Some(b),
        }
    }
}

/// Probe the host. Cheap enough to call when the page opens.
///
/// Asks the same API QEMU's own WHPX accelerator uses, rather than inferring
/// from Windows version or registry state — if `WHvGetCapability` says a
/// hypervisor is present, WHPX will initialise.
#[cfg(target_arch = "x86_64")]
pub fn probe() -> AccelStatus {
    if whpx_hypervisor_present() {
        return AccelStatus::Whpx;
    }
    // Order matters: firmware is the more fundamental failure, and its remedy
    // (a BIOS trip) is the one users are least likely to guess.
    if !firmware_virtualization_enabled() {
        return AccelStatus::SoftwareOnly(AccelBlocker::FirmwareDisabled);
    }
    if !platform_api_present() {
        return AccelStatus::SoftwareOnly(AccelBlocker::PlatformFeatureMissing);
    }
    AccelStatus::SoftwareOnly(AccelBlocker::HypervisorNotRunning)
}

/// Non-x86 hosts have no WHPX for x86 guests at all — the Virtualize feature
/// is hidden there, so this exists only to keep the crate building.
#[cfg(not(target_arch = "x86_64"))]
pub fn probe() -> AccelStatus {
    AccelStatus::SoftwareOnly(AccelBlocker::PlatformFeatureMissing)
}

/// `PF_VIRT_FIRMWARE_ENABLED`: "the processor supports virtualization and it is
/// enabled in the firmware".
#[cfg(target_arch = "x86_64")]
fn firmware_virtualization_enabled() -> bool {
    use windows_sys::Win32::System::Threading::{
        IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED,
    };
    // SAFETY: a pure query taking a feature id and returning BOOL.
    unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) != 0 }
}

/// Is `WinHvPlatform.dll` present? It ships with the Windows Hypervisor
/// Platform optional feature, so its absence means the feature is not enabled.
#[cfg(target_arch = "x86_64")]
fn platform_api_present() -> bool {
    with_whv_platform(|_| true).unwrap_or(false)
}

/// Ask WHPX directly whether a hypervisor is present.
#[cfg(target_arch = "x86_64")]
fn whpx_hypervisor_present() -> bool {
    /// `WHvCapabilityCodeHypervisorPresent`.
    const CAPABILITY_HYPERVISOR_PRESENT: u32 = 0;

    with_whv_platform(|module| {
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;

        // SAFETY: `module` is a live HMODULE for WinHvPlatform.dll.
        let Some(proc) = (unsafe { GetProcAddress(module, c"WHvGetCapability".as_ptr().cast()) })
        else {
            return false;
        };
        // HRESULT WHvGetCapability(UINT32 code, void* buffer, UINT32 size, UINT32* written)
        type WHvGetCapability =
            unsafe extern "system" fn(u32, *mut core::ffi::c_void, u32, *mut u32) -> i32;
        // SAFETY: signature matches the documented export; a mismatch would be
        // a Windows SDK change, not a runtime input.
        let get_capability: WHvGetCapability = unsafe { std::mem::transmute(proc) };

        // A 4-byte BOOL, not a 1-byte bool: `WHV_CAPABILITY`'s
        // `HypervisorPresent` member is a Win32 BOOL, and passing a smaller
        // buffer makes the call fail rather than truncate — which reads
        // exactly like "no hypervisor" and sends the user to their BIOS for
        // no reason.
        let mut present: u32 = 0;
        let mut written: u32 = 0;
        // SAFETY: buffer and size describe the same 4-byte BOOL the capability
        // writes; `written` receives the byte count.
        let hr = unsafe {
            get_capability(
                CAPABILITY_HYPERVISOR_PRESENT,
                (&mut present as *mut u32).cast(),
                core::mem::size_of::<u32>() as u32,
                &mut written,
            )
        };
        hr >= 0 && written >= core::mem::size_of::<u32>() as u32 && present != 0
    })
    .unwrap_or(false)
}

/// Load `WinHvPlatform.dll`, run `f`, and free it. `None` when the DLL is
/// absent — i.e. the optional feature is not installed.
#[cfg(target_arch = "x86_64")]
fn with_whv_platform<T>(f: impl FnOnce(windows_sys::Win32::Foundation::HMODULE) -> T) -> Option<T> {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryA;

    // SAFETY: a NUL-terminated ANSI name; a missing DLL returns null rather
    // than failing. Loading by bare name resolves through the system
    // directory, which is where this DLL lives.
    let module = unsafe { LoadLibraryA(c"WinHvPlatform.dll".as_ptr().cast()) };
    if module.is_null() {
        return None;
    }
    let out = f(module);
    // SAFETY: `module` came from LoadLibraryA and is not used after this.
    unsafe { FreeLibrary(module) };
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_is_consistent_with_itself() {
        // Can't assert a specific outcome — it depends on the machine this
        // runs on — but the two views must never disagree.
        let status = probe();
        assert_eq!(status.is_accelerated(), status.blocker().is_none());
    }

    #[test]
    fn every_blocker_says_what_to_do() {
        for b in [
            AccelBlocker::FirmwareDisabled,
            AccelBlocker::PlatformFeatureMissing,
            AccelBlocker::HypervisorNotRunning,
        ] {
            assert!(!b.headline().is_empty());
            // The remedies are the whole point: each must name a different
            // place to go, or the distinction we probed for is wasted.
            assert!(!b.remedy().is_empty());
        }
        assert_ne!(
            AccelBlocker::FirmwareDisabled.remedy(),
            AccelBlocker::PlatformFeatureMissing.remedy()
        );
    }
}
