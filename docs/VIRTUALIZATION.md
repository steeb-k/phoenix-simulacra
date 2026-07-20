# Booting a backup as a virtual machine

Boot a `.phnx` backup as a live VM — to verify a backup actually boots, recover data from a running OS, or keep working off a dead machine's last backup. The backup is never modified and is never materialized to disk: QEMU reads the guest's disk on demand through a WinFsp filesystem that synthesizes it from the `.phnx`, and every guest write lands in a copy-on-write overlay.

Simulacra drives an **external QEMU** with a generated command line rather than integrating a hypervisor. QEMU needs no service install (unlike the Hyper-V role or VirtualBox drivers), is scriptable, and is the only part of this feature not shipped in the app.

The feature is x86_64-only and absent from portable builds — see [Availability](#availability).

## Scope

**In:** UEFI/GPT and BIOS/MBR Windows guests booted from a single-disk capture; persistent, resumable sessions; rescue-ISO boot; host↔guest file exchange; guest tooling and clipboard; opt-in guest networking.

**Out:** multi-disk systems (one `.phnx` is one disk — a system spanning disks would need multi-backup sessions); TPM emulation, and therefore Windows Hello PIN; Secure Boot.

Deliberately **not built:** offline password or PIN bypass. Blanking a hash cascades into losing DPAPI-protected secrets sealed to the original password, patching the verification path is bootkit-shaped and trips antivirus, and neither helps against a Microsoft account. The locked-out-of-your-own-guest case is served by rescue-ISO boot instead.

## Architecture

### Read path

`WinFspServe` projects the synthesized disk image as a read-only VHDX **file** on a WinFsp filesystem — a real path any program can open. Every read is answered on demand from the `.phnx` and BLAKE3-verified. Serving does not attach the disk to Windows, so nothing surfaces on the host while the guest runs and there is no double-mount hazard.

**GPT identity must be boot-faithful.** The mount paths deliberately derive fresh disk and partition GUIDs as double-attach protection, but Windows' BCD references the OS partition by disk GUID plus PartitionId — derived GUIDs fail `winload.efi` with `0xc000000e`. `SyntheticVhd::build_vhdx_original_identity` / `WinFspServe::serve_for_vm` restore the manifest's original identity and GPT attribute bits. An image served this way must never be host-attached.

### Synthesizing an MBR disk

A capture of a BIOS/MBR disk gets a **real** MBR rather than GPT's protective one, from `phoenix-mount/src/mbr.rs`. The shape differs, not just the bytes: one leading sector instead of 34, and no trailing region at all where GPT reserves 33 sectors for its backup copy.

Three things have to be captured because nothing else in the backup contains them and none can be reconstructed afterwards:

| | Why it cannot be inferred |
|---|---|
| **NT disk signature** (4 bytes at `0x1B8`) | The BCD names the boot disk by it — the MBR analogue of the GPT disk GUID, and just as fatal to get wrong. |
| **Partition type byte** | The filesystem cannot distinguish a plain NTFS volume from a hidden `0x27` recovery partition; both are NTFS. |
| **Active flag** | A table with nothing active does not boot. |
| **Boot code** (bytes 0..440) | Lives *outside every partition*, so no captured partition holds it. Without it the BIOS executes zeros and the disk hangs however correct the table is. |

Four deliberate choices in the writer, each pinned by a test:

- A **zero type byte falls back to `0x07`** rather than being written through: zero means *unused*, so the guest would read an empty table — the same trap the GPT path avoids with Basic Data.
- **Overlong boot code is truncated** to 440 bytes so it can never reach the partition table.
- **Entries past the fourth are dropped**, not wrapped into the boot signature. An MBR holds four primaries; a backup with more came from an extended/logical layout this does not reproduce.
- With **no captured active flag** (only possible for backups predating the capture), the first small NTFS partition is marked — the System Reserved shape. Neither position nor size alone is right: a standard BIOS install puts `bootmgr` on a small System Reserved and leaves the large Windows volume inactive, while a single-partition install has it on the Windows volume itself. Verified against a real Windows 10 install, whose 50 MB System Reserved is active and whose 40 GB `C:` is not.

Validated end to end: a Windows 10 22H2 BIOS install, captured and booted from the `.phnx` to a desktop with the source detached. `phoenix-systests/tests/mbr_identity.rs` is the regression test — run it with `scripts\run-system-tests.ps1 -Test mbr_identity`.

### Write path: qcow2

The served VHDX is the read-only backing file of a per-session qcow2 (`qemu-img create -f qcow2 -b <served>.vhdx -F vhdx session.qcow2`), and QEMU does copy-on-write into the qcow2. No VHD attach happens anywhere in this path.

A raw-passthrough alternative exists (`WriteLayer::Avhdx`: a Windows differencing `.avhdx` attached offline, QEMU doing raw I/O against `\\.\PhysicalDriveN`) and is **experimental, off by default**. It boots fine; its *teardown* is unreliable. Detaching a differencing child whose parent lives on a WinFsp filesystem hangs in the Windows storage driver — measured across in-process and out-of-process serve, and across explicit `DetachVirtualDisk` and drop-based detach. A hang can leave an unkillable process needing a reboot, so qcow2 is the committed path: it makes the entire deadlock class structurally impossible.

The accepted consequence is that **mounts and VMs use different differencing engines** — file-level mounts use Windows differencing VHDX, VMs use qcow2. A VM session's files therefore cannot be browsed with the writable mount; that would need `qemu-nbd` or an offline `qemu-img` conversion.

### Sessions

A session is one backup plus one overlay plus its saved VM state: the qcow2, a per-session UEFI NVRAM varstore, and `session.json` (backup id, timestamps, resolved settings, clean/dirty flag). Guest writes are **kept** by default — stop the VM, come back later, resume — and discarded explicitly.

Everything a session creates lives on the **same volume as the `.phnx`**, under `<image-drive>:\PhoenixSimulacra\{vm-sessions,vm-serve}`. A backup on `D:` must never fill `C:`. The GUI offers a scratch-drive picker (with free space per volume) for cases where the image's drive is the wrong home for a growing overlay — a slow or nearly-full external, say — and the choice persists in settings. It governs **new** sessions only: resume and discard follow the session's own root, captured in `Session::vm_root()`, so changing the dropdown can never redirect an existing session. `simulacra-cli vm boot --vm-dir` is the CLI equivalent; `vm list` scans every fixed volume, since sessions follow their image's drive and there is no single root to search.

Only the overlay consumes real space. The served `backup.vhdx` is synthesized on demand, not materialized.

Crash discipline: `sweep_serve_scratch` is scoped to `vm-serve` and never touches kept overlays. It also reaps an orphaned serve helper, identified by a per-helper PID file and only terminated after confirming the process image name is really ours — PIDs get recycled.

## Auto-configuration from the manifest

The goal is zero-question boot: derive every setting from what the backup already knows. This is one pure function — manifest in, `VmConfig` out, then `VmConfig` to a command line — so the whole matrix is unit-tested without QEMU, WinFsp, or elevation.

| Manifest fact | QEMU setting |
|---|---|
| `is_gpt` = true | UEFI firmware (edk2/OVMF pflash pair), machine `q35` |
| `is_gpt` = false (MBR) | SeaBIOS (QEMU's default — no pflash), machine `q35` |
| `sector_size` = 4096 | `logical_block_size=4096,physical_block_size=4096`, NVMe controller (IDE and AHCI are 512-byte only) |
| Partition `fs_kind` (NTFS/ReFS + ESP) | AHCI `ide-hd` on q35 — Windows' inbox `storahci` boot driver, so no virtio injection and no `0x7B` |
| `drive_type` = SSD | `rotation_rate=1`, so the guest doesn't schedule defragmentation |
| BitLocker locked | **Refuse to boot** — a TPM-sealed volume cannot unseal on virtual hardware anyway |
| BitLocker unlocked at capture | Boots from the plaintext capture; warn that the guest may prompt for a recovery key on later lock/unlock cycles |

Host-side knobs (`HostOptions`): memory, vCPUs, acceleration, scratch drive, and the opt-in flags below. Memory and CPU sliders are colour-coded against the host's *available* RAM and total cores, and persist across launches.

## The QEMU recipe

Every element below is load-bearing; the reasons are recorded because they were expensive to find.

- **`-machine q35`**, **`-accel whpx`** with the **default IRQ chip**. Passing `kernel-irqchip=off` wedges Windows at early kernel init — no bugcheck, no I/O, roughly 450 MB in — through userspace-APIC timer starvation.
- **A rich named CPU model** (`Skylake-Client-v4`), which clears Windows 11's SSE4.2/POPCNT floor. **Never `-cpu max`** under WHPX: on APX-era hosts it exposes features the hypervisor cannot virtualize and the guest spins before video init.
- **Writable copies of both pflash units.** Attaching the OVMF code flash `readonly=on` maps it as ROM, and **WHPX cannot execute from ROM-mapped memory** — OVMF then burns 100% CPU and never reaches video init.
- **`bootindex` pinned on the OS disk.** Without it a fresh UEFI varstore spends minutes on PXE and HTTP boot attempts and drops to the UEFI shell without ever trying the disk.
- **The NVRAM varstore is per-topology session state.** Reuse it only while the disk controller layout is unchanged, or stale `Boot####` entries dangle.
- **`-vga none -device virtio-vga`, always.** Two reasons. Recent QXL-DOD builds *claim* QEMU's stdvga and render black at WDDM handoff, whereas virtio-vga is driven by the maintained viogpu driver — and boots fine driverless too, via Basic Display over the GOP framebuffer. And its firmware GOP tops out at 1920x1080, which combined with `zoom-to-fit` retires an older VRAM-capping hack entirely. The device must stay **stable across a session's boots**; swapping it makes the guest re-detect hardware.
- **`-display gtk,zoom-to-fit=on`**, with `clipboard=on` appended only when the QEMU in use supports it. The window is maximized after launch (QEMU has no start-maximized switch — `full-screen=on` is borderless fullscreen, a different thing) by a watcher that finds the process's main window with `EnumWindows`.
- **A CD gets its own SATA port** (`ide-cd,...,bus=ide.1`). q35 AHCI ports are single-unit, and QEMU auto-places an unbussed CD as unit 1 of the port the OS disk owns, then refuses to start. The drivers ISO takes `ide.2` and the helper disk `ide.3`; neither is ever given a `bootindex`.
- **A QMP socket on an ephemeral loopback port**, recorded in the session. It carries graceful shutdown (`system_powerdown`), the network link toggle (`set_link`), and the immediate power-off used when the app closes.

qemu-system and qemu-img are console applications; every spawn passes `CREATE_NO_WINDOW` and redirects output to a per-launch `qemu.log` in the session directory, whose tail is surfaced in the UI when a launch fails.

### When WHPX is unavailable

QEMU is launched with `-accel whpx -accel tcg`, an ordered fallback list, so a machine without hardware acceleration still boots — in software emulation, which for a Windows guest is the difference between minutes and hours. Silent fallback is worse than useless on its own: the user waits, concludes the feature is broken, and never learns it is one checkbox away.

So `phoenix-vm/src/accel.rs` probes up front, asking `WHvGetCapability` — the same API QEMU's own WHPX accelerator uses — rather than inferring from Windows version or registry state. The DLL is loaded dynamically, since `WinHvPlatform.dll` ships with an *optional* Windows feature and its absence is itself one of the answers.

The three outcomes need completely different actions, which is the whole reason for distinguishing them:

| Blocker | Remedy | Fixable in-app |
|---|---|---|
| Virtualization disabled in **firmware** | Reboot into BIOS/UEFI setup | No — no amount of elevation reaches it |
| **Windows Hypervisor Platform** not enabled | A Windows Features checkbox | Yes (DISM) |
| **Hypervisor not running** | `bcdedit /set hypervisorlaunchtype auto` | Yes |

The two Windows-side fixes are applied from the app, which already runs elevated. Both need a restart, so success reports "restart to take effect" rather than re-probing — a re-probe would still say unavailable and make a working fix look like a failure. A single "enable virtualization" message would send half of these users to the wrong place.

`--demo-accel=firmware|platform|hypervisor` forces each state, since a development machine never shows them.

Note the buffer size when reading the capability: `WHV_CAPABILITY.HypervisorPresent` is a 4-byte `BOOL`. Passing a smaller buffer makes the call *fail*, which reads exactly like "no hypervisor" and sends the user to their BIOS for nothing.

### Guest restarts

**WHPX cannot reset a virtual processor in place.** A guest-initiated reboot — Windows Restart, exiting PE, a bugcheck auto-restart — surfaces as `WHPX: Unexpected VP exit code 4` in `qemu.log`, and QEMU then either exits cleanly or wedges alive with the scanout dead. Either way the user sees a restart kill the VM.

`boot()` handles both. It relaunches QEMU against the same session when it sees that signature (the overlay, varstore and QMP port all persist), guarded against crash-looping by a 15-second minimum uptime and a 32-relaunch ceiling; a watchdog kills a QEMU that wedges rather than exits. The relaunch drops `boot_iso_first`, which makes the one-shot ISO semantics *more* correct: restarting out of a rescue ISO lands on the disk.

## Guest networking

**Networking is opt-in and granted after boot, never at boot.** A connected Windows guest is offered a display driver by Windows Update within moments of first login, and that driver blanks the screen on the next reboot (see [Guest tools](#guest-tools-and-drivers)). Booting offline avoids the race entirely.

The NIC always exists; what changes is whether its cable is plugged in. The running-VM view has a Connect/Disconnect button that issues QMP `set_link` — instant, no reboot, nothing done inside the guest. Link state is host-side, so a guest cannot plug itself in, which makes an unplugged NIC as isolating as an absent one while leaving something to connect.

This is why the NIC is spelled out as `-netdev user,id=net0` plus `-device e1000e,netdev=net0,id=nic0` rather than the `-nic` shorthand: `set_link` addresses the device by name, and `-nic` generates an id that cannot be referred to reliably.

One caveat: neither `e1000e` nor `virtio-net-pci` exposes a link-down property to start with, so the cable can only be pulled once the machine exists. `boot()` does it the moment QMP answers, retrying briefly. That window is milliseconds wide and sits in the firmware phase, long before a guest OS brings up a network stack — but it is a window, not a guarantee.

**slirp cannot express "host yes, internet no".** `restrict=on` is total isolation and drops guest→`10.0.2.2`, taking the SMB share with it. The `guestfwd` punch-through is a dead end: its chardev form is a single shared stream with no per-connection demultiplexing, so SMB's multiple connections would corrupt each other. The viable design is a Windows Firewall outbound rule scoped to `qemu-system-x86_64.exe`, since slirp turns guest→`10.0.2.2` into a loopback connection while internet traffic is an ordinary outbound socket from the same process. Unbuilt; loopback's exemption from WFP filtering wants verifying first.

## Host↔guest file exchange

Windows already runs an SMB server on port 445, and a slirp guest reaches the host's loopback at the gateway address `10.0.2.2`. So the share needs no new server: the app `net share`s a folder next to the image at boot (it is already elevated), re-points it per run, and removes it at teardown. One share at a time, matching one VM at a time.

The guest maps it with:

```text
net use S: \\10.0.2.2\SimulacraShare /user:HOSTNAME\user /persistent:yes
```

authenticated with the host account's password — on a Microsoft-account PC, the Microsoft-account password. Mapping cannot be automatic, because the guest cannot be reached before it boots.

Saving the credential as well is not possible with one command: `net use` refuses to combine `/savecred` with `/persistent:yes`, so a mapping can be remembered or its credential can be, never both. Storing it separately with `cmdkey` parses but does not help — `net use` prompts anyway, so the guest asks twice rather than once. The mapping is remembered; the password is re-entered after a guest reboot.

### The VMSCRIPTS helper disk

Typing that command inside a guest with no clipboard is miserable, so every boot also builds a small **real FAT16 image** (`share::build_helper_disk`) attached as a never-booted disk on `ide.3`, labelled `VMSCRIPTS`. It carries `MapShare.cmd` (map the share and open Explorer on it), `InstallGuestDrivers.cmd`, and a README.

It is a real image rather than QEMU's VVFAT because writable VVFAT is corruption-prone and IDE disks cannot attach read-only; guest writes to this image land in a throwaway per-session file instead.

The image needs a **real MBR partition** — type `0x0E`, starting at LBA 2048. Windows mounts partitionless "superfloppy" layouts only from removable media; on the fixed disk `ide-hd` presents, one appears as an uninitialized disk with no drive letter.

The helper disk is **never** gated on the share or on networking. It carries the driver installer, which matters most precisely when the guest is offline.

Alternatives rejected for a simultaneously-writable channel: virtio-fs and 9p (no Windows-host daemon), writable VVFAT (experimental, corrupts), a dual-mounted disk image (two operating systems on one filesystem is guaranteed corruption), and WebDAV via an in-app server — the only real non-SMB option, using the guest's inbox client at `http://10.0.2.2:port/`, but a sizeable LOCK-compliant server build and slower.

## Guest tools and drivers

virtio-win ships as an ISO, which sidesteps the share-needs-drivers / drivers-need-share problem entirely. The GUI offers "Attach guest tools and driver ISO", downloading virtio-win on demand next to the app with a progress bar and a HEAD-comparison update check, and attaching it as a second never-booted CD on `ide.2`.

**Install it with `InstallGuestDrivers.cmd` from the VMSCRIPTS drive**, which self-elevates, locks Windows Update down, disables Fast Startup, and then launches the CD's own installer.

Three details in that script are load-bearing:

- **It runs `virtio-win-guest-tools.exe`, the bundler — not `virtio-win-gt-x64.msi`.** Only the bundler carries the SPICE vdagent, so clipboard cannot work without it. (vdagent was absent from virtio-win entirely for several releases before returning in 0.1.262; want 0.1.262 or newer.)
- **It does not install silently, and does not install `qemu-ga` unconditionally.** A hand install works reliably where a scripted one has produced black screens; a `/quiet` run takes bundle defaults where an interactive one can decide per device, and the bundler already ships `qemu-ga`, so an unconditional second pass ran two agent installs back to back while driver staging was still settling. The script launches the same installer a user would and only adds `qemu-ga` if the service is genuinely absent.
- **Windows Update driver delivery is blocked *before* the install**, via `DontSearchWindowsUpdate`, `SearchOrderConfig=0`, `ExcludeWUDriversInQualityUpdate` and `NoAutoUpdate`. Applied afterwards, the policy has already lost the race on a connected guest. The service disables are belt-and-braces only — Windows revives `wuauserv` on its own, so the policy keys carry the load.

Fast Startup is disabled (`HiberbootEnabled=0`, plus `powercfg /h off`) because it makes shutdown a kernel hibernate rather than a real shutdown: the guest skips hardware re-enumeration between boots and leaves NTFS dirty in the overlay, both bad for a machine whose virtual hardware can change underneath it.

### Black screens after reboot

Full-Windows guests used to go black immediately after the boot spinner on every reboot following a guest-tools install. Firmware rendered — BCD picker, spinner — and the guest stayed healthy (QMP `query-blockstats` showed steady I/O), but the scanout was black and it ignored wake keys and `Win`+`P`. It survived resumes, so the state lived in the overlay.

**The cause is the display driver Windows Update delivers**, not the SPICE vdagent and not the driver CD's own display driver, both of which were blamed first. Proven by booting offline and installing the full virtio-win suite, GPU driver included: it reboots indefinitely without incident. This is why networking is opt-in and why the script blocks WU driver delivery before installing anything.

## Clipboard

Host↔guest clipboard needs **QEMU 11.1 or newer** and the guest-tools bundler. Both halves are required and neither works alone: `clipboard=on` on the GTK display is the host-side peer, and the `qemu-vdagent` chardev is what the guest's agent talks to. No SPICE server or client is involved — `qemu-vdagent` implements enough of the agent protocol in-process.

`ui/gtk-clipboard.c` used to sit behind the compile-time `--enable-gtk-clipboard`, default-off upstream and labelled "EXPERIMENTAL, MAY HANG" because clipboard retrieval was blocking and hung the guest. **No Windows distributor enabled it.** QEMU 11.1 fixed the hang and replaced the build flag with a runtime option.

Do not infer support from a version string. Windows QEMU builds are branch snapshots whose numbers do not track upstream tags — a Weilnetz snapshot self-reporting `11.0.50` *is* the 11.1 development tree, while one reporting a plain `11.0.0` landed exactly on the release tag days before the clipboard commit. `Qemu::gtk_clipboard` therefore **probes**, by parse-checking `-display gtk,clipboard=on` against `-machine help` and reading the exit status.

Keyboard capture is a related trap worth recording: installing the guest tools is what *stops* the VM grabbing input, because the agent gives the guest absolute pointer positioning and QEMU no longer needs to grab anything. Hotkeys then reach the host instead of the guest. `Ctrl`+`Alt`+`G` toggles capture manually and works reliably; `grab-on-hover` was tried as a setting and removed, because it captured unreliably *and* broke `Ctrl`+`Alt`+`G` while enabled.

## Rescue-ISO boot

Attach an ISO and boot from it on the next boot only — to drop into WinPE, a Windows recovery ISO, or a live Linux, inspect or repair the captured drive from the outside, then fall back to booting the disk.

This is nearly free in this model. A fresh QEMU process launches on every boot, so "next boot only" is not persistent state to track and revert; it is a per-launch flag. That launch adds a read-only CD and gives it boot priority, and the next launch simply does not. The disk overlay stays attached throughout, so the rescue environment sees the guest's own drive and any repair it makes lands in the session overlay, never in the `.phnx`.

## QEMU discovery and bundling

Search order: an explicit setting (`Settings::vm_qemu_dir`, or `--qemu-dir`) → the **bundled** copy beside the app → `PATH` → the stock install locations. The bundled copy is preferred over anything on the system because it is the build the app was validated against, and a system QEMU may be older and silently lose features — but an explicit directory still wins, so a user who chose their own install keeps it. A directory only becomes the saved choice if `Qemu::discover` succeeds on it, so a wrong folder cannot be silently persisted.

QEMU is installed **privately**, into `<install dir>\qemu`, never `C:\Program Files\qemu`: a QEMU the user installed themselves must never be touched, overwritten or version-clashed with. The location is changeable in the UI.

### The payload

Upstream ships one installer carrying 55 system emulators and firmware for all of them — 1,170 MB installed. Simulacra launches exactly one target, and NSIS silent installs take default component selection with no component flags to override it, so `scripts/build-qemu-payload.ps1` extracts the upstream installer and prunes it to **223 MB** (84 MB compressed). That script is run by hand when moving QEMU versions, never as part of a release build; its output is published to the `phoenix-simulacra-deps` repository and pinned by SHA-256.

Because the pruning is ours rather than upstream's, the script proves what it produced: the expected version, `-display gtk,clipboard=on` still accepted, `qemu-img` runs, and a paused machine starts with **the exact device set the app emits**. A missing option ROM or firmware blob fails the build rather than a user's first boot.

The installer **downloads it at install time** rather than embedding it — 84 MB in every setup executable would be paid on nearly every auto-update, for a payload that changes far more rarely than the app. A failed download is deliberately not fatal: the Virtualize page offers the same download later, so an offline or firewalled machine still gets a working app. Both paths unpack with the inbox `tar.exe`; Inno's own archive extraction was tried and refused the payload outright.

The download is **pin-driven, not latest-driven** — it fetches the exact validated version rather than asking upstream what is newest. QEMU version changes alter the boot recipe, so moving builds is an app-release decision made after testing, and bumping the pin (in `installer/simulacra.iss` and `phoenix-gui/src/qemu_payload.rs`, which must agree) is how a new one ships.

**The bundled build must be QEMU 11.1 or newer**, or host↔guest clipboard silently disappears.

### Licensing

QEMU is **GPLv2**. It runs as a separate process driven over a command line, so it is not linked into the application and does not affect this project's licensing. Redistributing its binaries does carry the *corresponding source* obligation: each payload release links the exact upstream commit, and the licence text ships inside the payload as `COPYING` / `COPYING.LIB`.

## Throughput

A booting guest issues scattered 4 KiB reads, and every chunk-cache miss decompresses a whole 4 MiB chunk. Three pathologies in `phoenix-mount::chunkstore` made this dominate boot time; all are fixed:

1. **Every read cloned the whole `ExtentSpan`**, including a `Vec<PlacedChunk>` whose chunks each carried a heap hex `String` hash — tens of thousands of allocations per read on a large partition. Chunks are now located under an immutable borrow, with only the (heap-free, `Copy`) `PlacedChunk` copied out, and hashes stored decoded as `[u8; 32]`.
2. **Every cache hit copied the whole 4 MiB chunk** to serve a 4 KiB read. The cache now holds `Arc<Vec<u8>>`, so a hit is a refcount bump.
3. **The cache was FIFO, not LRU** — `cache_order` was never touched on a hit, so hot chunks were evicted on a fixed schedule and re-decompressed and, on a spinning disk, re-*seeked*. Now promote-on-hit, plus sequential-detection readahead.

Measured with the backup on a 7200-rpm HDD, release build:

| Workload | Before | After |
|---|---|---|
| Random 4 KiB, cold | 52 reads/s (19.1 ms) | **200 reads/s (5.0 ms)** |
| Random 4 KiB, warm | 136 reads/s (7.4 ms) | **517 reads/s (1.9 ms)** |
| Sequential | 131 MB/s | 132 MB/s (already streaming) |
| Boot to lock screen | ~13 min | **~3 min** |

Two lessons worth keeping. **The backing store matters more than the CPU here:** on an HDD a cache miss costs a ~10 ms seek, so the wins came from *avoiding* I/O, not decoding faster — parallel chunk decode would have been largely wasted and parallel I/O on a spindle would have hurt. And **measure the right workload**: a sequential benchmark looked fine at 131 MB/s because it amortizes per-read cost across a whole chunk; only a scattered 4 KiB benchmark exposed the problem.

Still available if more is needed: reads serialize on one mutex (`VhdFs::read`), so there is no read concurrency. That matters little on an HDD; on an SSD-backed image it is the next lever. Putting the `.phnx` on an SSD is simply faster too — another reason for the scratch-drive picker.

## Availability

**ARM64 builds hide the Virtualize page entirely.** Windows-on-ARM QEMU has no useful acceleration for x86 guests — TCG emulation only, unusably slow for Windows — and ARM-Windows guests vary too much in guest-visible hardware for a booted backup to be dependable. Hiding the page beats shipping a trap.

**Portable builds hide it too.** Booting a backup needs an installed WinFsp to serve the disk and an installed QEMU to run it, and installing nothing is the entire point of the portable build. A page that could only ever report missing prerequisites is worse than no page.

Both are decided by `sidebar::page_available`, which also guards the `--page virtualize` argument and falls back to the Backup page if the current page ever becomes unavailable.

## Safety rails

- **A running VM freezes navigation** to the Virtualize page, the way a running job does. It is the only place to see or stop the VM.
- **Closing the app is held for a hazard-tape confirmation** when a VM is running. The app hosts the WinFsp filesystem serving the guest's disk, so closing it pulls that disk out from under a live guest — the VM does not die cleanly, it degrades. Confirming powers the VM off immediately (QMP `quit` — a deliberate power cut, since a graceful shutdown cannot finish once its disk is gone), then unmounts, then closes.

## Limitations

- **Bootloaders using the MBR gap are not fully captured.** The space between LBA 0 and the first partition — where GRUB stages its second stage — lies outside every partition and is not captured; only the MBR's own 440 bytes of boot code are. Windows is unaffected, keeping `bootmgr` in System Reserved's boot sector, which is inside a captured partition.
- **Fast Startup produces a hibernated capture.** Windows enables it by default, so "Shut down" writes a kernel resume image to `hiberfil.sys` rather than shutting down. A guest booted from such a capture tries to resume and may fail — observed as a crash just after the Windows logo. Disabling Fast Startup on the source before capture avoids it; `InstallGuestDrivers.cmd` disables it inside the guest, so only the first boot of a session is exposed.
- **No TPM emulation**, so Windows Hello PIN is unavailable in the guest; password sign-in works. Secure Boot is off. Windows may demand reactivation.
- **NVMe is unverified on current QEMU.** It is required for 4Kn captures, since IDE and AHCI are 512-byte-sector only, but QEMU's `nvme` device was invisible to one build's OVMF even with an explicit `nvme-ns`. Retest before relying on 4Kn boot.
- **One `.phnx` is one disk**; multi-disk systems are out of scope.
- **Acceleration falls back to TCG** if WHPX is unavailable — correct but very slow for a Windows guest. The Virtualize page says why and offers the fix; see [When WHPX is unavailable](#when-whpx-is-unavailable).

The original exploratory notes for this feature are kept in [QEMU-BOOT.md](QEMU-BOOT.md).
