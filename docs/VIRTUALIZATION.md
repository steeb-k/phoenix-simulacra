# Virtualization: booting backups as virtual machines

Status: **exploration branch** (`virtualization`). This supersedes and
concretizes the earlier sketch in [QEMU-BOOT.md](QEMU-BOOT.md); that document
stays as the original Phase 2 notes until this plan settles.

## Goal

Boot a `.phnx` backup as a live VM — verify a backup actually boots, recover
data from a running OS, or keep working off a dead machine's last backup —
without modifying the backup and without materializing it to disk.

Three decisions are taken as premises for this branch:

1. **QEMU wrapper, not a hypervisor integration.** We drive an external QEMU
   with an auto-generated command line. QEMU is open-source, scriptable,
   cross-platform, and needs no service install (unlike Hyper-V role or
   VirtualBox drivers). We do not bundle it initially — we detect it (see
   below).
2. **Reuse the write-overlay machinery from the writable mount.** The backup
   stays immutable; all guest writes land in a copy-on-write layer, exactly
   like "Enable Write Access" on the Mount page.
3. **Sessions are persistent and tracked.** Unlike the writable mount's
   ephemeral child (deleted on unmount), a VM session's overlay is *kept* by
   default and tracked, so the user can shut the VM down, come back tomorrow,
   and continue — and explicitly discard the session when done with it.

## Where it lives: in the main app, as a new crate

Recommendation: a new workspace crate **`phoenix-vm`** (library) wired into
`phoenix-cli` (a `vm` subcommand) first and the GUI (a "Boot as VM" action)
second — not a separate application.

Rationale:

- The read path *is* `phoenix-mount` (`WinFspServe` + `SyntheticVhd` +
  `ChunkStore`). A separate app would either duplicate that stack or grow an
  IPC layer for no benefit.
- Session bookkeeping wants the same scratch-dir, cleanup-on-crash, and
  "orphan sweep" discipline the mount code already has (`clean_stale_mount_dirs`).
- The GUI already has the pattern for this: a page plus `current_page_action`
  for the sticky Start bar, same as Mount/Clone/Boot-repair.
- If it ever needs to ship separately, the crate boundary makes that cheap.
  The reverse (extracting it out of a monolith later) is not.

The one thing that stays external is QEMU itself.

## Architecture

### Read path (settled)

`WinFspServe` already projects the synthesized image as a **read-only VHDX
file** on a WinFsp filesystem — a real path any program can open. Today that
file is used as the differencing parent of the writable mount's `.avhdx`
child; for VM use, the same served file is the VM disk's immutable base. Every
read is answered on demand from the `.phnx` and BLAKE3-verified, footprint-free.

Serving does not require attaching the disk to Windows, so nothing surfaces in
the host while the guest runs — no double-mount hazard.

### Write path — DECIDED: qcow2 (Option B)

> **Resolved 2026-07-19: qcow2 is the committed write path; Option A
> (`.avhdx` raw passthrough) is EXPERIMENTAL and off by default.**
>
> Option A boots Windows fine, but its **teardown is unreliable**: detaching a
> differencing child whose parent lives on a WinFsp filesystem hangs in the
> Windows storage driver. Measured across four configurations — in-process and
> out-of-process serve, explicit `DetachVirtualDisk` and drop/`CloseHandle`
> detach — **all four hang**; only one much earlier light-I/O spike ever
> detached cleanly. A hang could leave an unkillable process needing a reboot.
> qcow2 performs **no VHD attach anywhere**, so the entire deadlock class is
> structurally impossible, and it is verified end-to-end (boots Windows 11;
> lifecycle + resume green in `vm_overlay_lifecycle`).
>
> Consequence, accepted deliberately: **mounts and VMs use different
> differencing engines** — file-level mounts keep Windows differencing VHDX,
> VMs use qcow2. The boot-it-or-mount-it interop that motivated Option A is
> given up; browsing a VM session's files would need another route (expose the
> qcow2 via `qemu-nbd`, or an offline `qemu-img` conversion/export).
>
> The out-of-process serve helper built while chasing this is kept: it didn't
> fix the detach, but it turned a hang from "unkillable process, reboot" into
> "kill one helper, done".

Constraint discovered up front: **QEMU's `vhdx` block driver does not support
differencing (parented) VHDX images.** So QEMU cannot do its I/O directly
against a `child-*.avhdx`. The two shapes considered:

**Option A — raw physical-drive passthrough over the existing `.avhdx` child
(EXPERIMENTAL; teardown unreliable — see the box above).** Reuse the shipped
overlay *literally*:

1. Serve the parent VHDX via `WinFspServe` (as today).
2. Create/open the session's differencing child (`create_differencing_vhdx`,
   as today) and attach it **read-write with no drive letters and the disk
   forced offline**, so the host's filesystem stack never touches the volumes.
3. Point QEMU at the attached raw disk: `\\.\PhysicalDriveN`,
   `format=raw`. Guest writes flow through the Windows storage stack into the
   `.avhdx`; the parent stays read-only (`RWDepth = 1`, same as the writable
   mount).

Pros: one overlay format for everything; a VM session's `.avhdx` can later be
mounted with the *existing* writable mount to browse/extract files (VM boot
and file-level mount become two views of the same session — not
simultaneously, enforced by session state). All the attach/cleanup code
exists. Cons: Windows-only, requires admin (the app already runs elevated),
and QEMU-on-raw-physical-device is a less common configuration to get right
(caching flags, exclusive access).

**Option B — qcow2 overlay over the served VHDX.** QEMU-native: the served
parent VHDX is the read-only backing file of a qcow2
(`qemu-img create -f qcow2 -b <served>.vhdx -F vhdx session.qcow2`), QEMU does
CoW into the qcow2. Pros: the boring, extremely well-trodden QEMU path; no
disk attach at all; qcow2 internal snapshots come free; portable off-Windows
someday. Cons: a *second* overlay format — a qcow2 session cannot be browsed
by the Windows writable mount (only by booting the VM, or an offline
`qemu-img` conversion); depends on QEMU's VHDX reader handling our synthesized
VHDX (needs an early spike to confirm).

Both were spiked in milestone 2 and both booted Windows. A was chosen first for
its session-interop appeal, then **reversed** once its teardown proved
unreliable (box above). B — qcow2 — is the committed path, and is also the
eventual non-Windows story.

### Session model

A **VM session** = one backup + one overlay + one saved VM config.

- Stored under a per-backup sessions directory next to the scratch dir:
  overlay file + `session.json` (backup id, created/last-booted timestamps,
  the resolved VM settings, dirty/clean shutdown flag).
- Footprint = overlay growth only — the mount-space rule (never double a
  backup's footprint) applies here unchanged.
- Lifecycle verbs: `boot` (create session if none, else resume), `list`,
  `discard`, and later `mount` (open the session's overlay in the writable
  mount, Option A only) and possibly `commit` (fold a session into a new
  `.phnx` — explicitly out of scope for this branch, noted so we don't design
  it out).
- Crash discipline: sessions survive crashes by design (they're persistent),
  but a stale *attach* or WinFsp serve from a dead process gets swept exactly
  like orphaned mount state today.

## Auto-configuration from the manifest

The wrapper's job is "zero-question boot": derive every QEMU setting we can
from what the backup already knows.

| Manifest fact | QEMU setting |
|---|---|
| `is_gpt` = true | UEFI firmware (edk2/OVMF `-drive if=pflash` pair — the Windows QEMU packages ship `edk2-x86_64-code.fd`), machine `q35` |
| `is_gpt` = false (MBR) | SeaBIOS (default), machine `q35` still fine; `pc` as compat fallback |
| `sector_size` = 4096 | `logical_block_size=4096,physical_block_size=4096` on the disk device (the guest saw 4Kn on real hardware; presenting 512e would break NTFS boot geometry assumptions) |
| `size_bytes` | disk size (implicit from the base image) |
| Partition `fs_kind` (NTFS/ReFS + ESP present) | Windows guest heuristics: AHCI controller (inbox `storahci` driver — avoids virtio 0x7B), `e1000e` NIC (inbox driver), `-cpu` with Hyper-V enlightenments |
| Partition `fs_kind` (ext4/xfs/btrfs…) | Linux guest heuristics: virtio-blk + virtio-net directly (kernels carry the drivers), serial console available |
| BitLocker state = locked | **Refuse to boot**, same message as the writable mount: a TPM-sealed volume cannot unseal on virtual hardware anyway |
| BitLocker state = unlocked-at-capture | Boots (plaintext capture), but warn: guest may flip to recovery-key prompts on next lock/unlock cycles |
| `drive_type` (SSD vs HDD) | `rotation_rate=1` for SSD-captured disks so the guest doesn't try to defrag/optimize as HDD |

Host-side, not manifest-side:

- **Acceleration:** probe for WHPX (`-accel whpx,kernel-irqchip=off`) and fall
  back to TCG with a loud "this will be slow" warning. On ARM64 hosts there is
  no WHPX for x86 guests at all — TCG only; document rather than block.
- **RAM/CPUs:** default to min(half of host RAM, 8G) and min(host cores, 4),
  user-overridable. These are the only knobs surfaced in the initial UI.
- **Display/network:** QEMU's default GUI window; user-mode networking (slirp)
  — no host network changes, guest gets outbound + DHCP, no inbound. A
  "no network" toggle for forensic booting of possibly-compromised images.

Everything above lands in one pure function — manifest in, `VmConfig` out,
then `VmConfig → Vec<String>` for the command line — so the whole matrix is
unit-testable with snapshot tests, no QEMU needed (T1-friendly).

## QEMU discovery

Search order: explicit setting (GUI settings / `--qemu-path`) → `PATH` →
default install locations (`C:\Program Files\qemu`, the MSYS2 locations).
Validate with `qemu-system-x86_64 --version` and record the version (feature
gates: WHPX flags changed across versions). If not found, the GUI shows a
"QEMU required" call-to-action with a link rather than failing cryptically.

**Bundling (decided): ship the QEMU installer in our installer, WinFsp-style.**
Licensing is fine: QEMU is GPLv2, and bundling its unmodified installer next
to our app is mere aggregation — it imposes nothing on our license, same as
the WinFsp MSI we already ship. The GPL obligation we do pick up is
*corresponding source*: pin the exact installer build we bundle and link (or
archive) its matching source tarball in the release notes. Practicalities:
the installer is large (~150–300 MB, a big jump for our installer), so the
detect-first logic above stays — bundled QEMU installs only when none is
found — and the runtime path still honors a user-supplied QEMU.

## Known hard parts (carried over, still true)

- **`INACCESSIBLE_BOOT_DEVICE` (0x7B)** for physical-Windows captures whose
  boot-critical storage driver doesn't match the virtual controller. AHCI
  first (inbox driver on everything since Vista); virtio driver injection is a
  later, optional "prep" feature, not milestone 1.
- **Firmware NVRAM:** a UEFI guest wants a writable NVRAM varstore; per-session
  copy of `edk2-i386-vars.fd` lives in the session directory (it's part of the
  tracked session state — boot order changes persist with the session).
- **TPM / Secure Boot / activation:** no TPM emulation initially (swtpm is
  awkward on Windows); Secure Boot off; Windows may demand reactivation.
  Documented limitations.
- **Multi-disk systems:** one `.phnx` is one disk. Booting a system that
  spans disks needs multi-backup sessions — out of scope, note in docs.

## Boot-smoke findings (2026-07-18 — milestone 5 reached same-day)

A real Windows 11 backup (106 GiB GPT system disk, BitLocker-unlocked capture)
**booted to the login screen** in QEMU, served on demand from the `.phnx`
(qcow2 write path). The validated recipe, and every trap found on the way:

- **GPT identity must be boot-faithful.** The mount paths derive fresh
  disk/partition GUIDs (double-attach protection), but the BCD references the
  OS partition by disk GUID + PartitionId — derived GUIDs fail `winload.efi`
  with 0xc000000e. `SyntheticVhd::build_vhdx_original_identity` /
  `WinFspServe::serve_for_vm` restore the manifest's original identity (plus
  GPT attribute bits). Never host-attach an image served this way.
- **WHPX cannot execute from ROM-mapped pflash.** OVMF attached with
  `readonly=on` never reaches video init (100% CPU trap-storm). Fix: give
  QEMU per-session **writable copies** of both firmware flashes.
- **Never `-cpu max` under WHPX** on new hosts (APX-era): exposes features the
  hypervisor can't virtualize and the guest spins pre-video. Use a rich named
  model (`Skylake-Client-v4` clears Win11's SSE4.2/POPCNT floor).
- **Don't pass `kernel-irqchip=off`.** With it, Windows wedges at early kernel
  init (~450 MB in, no I/O, no bugcheck) — userspace-APIC timer starvation.
  Default WHPX irqchip boots clean.
- **Pin the OS disk with `bootindex=0`**, or a fresh OVMF varstore burns
  minutes on PXE/HTTP and drops to the UEFI shell without trying the disk.
- **NVRAM varstore is per-topology session state**: reuse it only while the
  disk controller layout is unchanged, else stale Boot#### entries dangle.
- **AHCI (`ide-hd` on q35) boots Windows 11 fine** — the inbox `storahci`
  driver came up on a capture from NVMe hardware; no 0x7B, no injection
  needed on this guest. QEMU's `nvme` device was invisible to this build's
  OVMF even with explicit `nvme-ns` (dev-snapshot suspicion) — retest on
  stable QEMU; NVMe remains required for 4Kn (IDE is 512-only).
- **Use a stable QEMU release.** winget's SoftwareFreedomConservancy.QEMU was
  a master-branch dev snapshot (11.0.50) with a broken/hanging OVMF; the
  bundled-installer decision above should pin a known-good stable build.
- **Throughput is the felt bottleneck**: a booting guest issues scattered 4K
  reads, and every chunk-cache miss decompresses a whole 4 MiB chunk (up to
  1000x amplification). Raising the chunkstore cache to 1.5 GiB made boots
  practical; the real fix is readahead + parallel chunk decode (ROADMAP P3).
  Boot-to-login was ~4 minutes on this rig, wall-clock dominated by that.
- **TPM caveat confirmed as predicted**: Windows Hello PIN unavailable in the
  guest (TPM-sealed); password sign-in works.
- **Known gap:** MBR/BIOS captures — the synthesis currently always presents
  GPT, so a BIOS-mode guest can't boot its MBR layout yet. Needs MBR
  synthesis before SeaBIOS boots are real.

## Milestones

1. **Plan + branch** (this document).
2. **Write-path spike (decision point):** hand-drive both Option A (offline
   attach + `\\.\PhysicalDriveN` raw) and Option B (qcow2 over served VHDX)
   against a small Linux backup; measure stability and I/O sanity; pick.
3. **`phoenix-vm` crate:** QEMU discovery, `VmConfig` synthesis from the
   manifest (unit-tested matrix), command-line builder, process
   lifecycle + teardown.
4. **Sessions:** persistent overlay + `session.json`, resume/discard,
   orphan sweep. CLI: `simulacra-cli vm boot|list|discard`. **DONE** — the
   `phoenix-vm` crate (`config`/`qemu`/`session`/`boot`) + the CLI subcommand
   ship the session manager; sessions live under
   `%LOCALAPPDATA%\PhoenixSimulacra\vm-sessions\{backup_id}\`, keyed so resume
   reuses the existing overlay. Validated: `vm boot` boots the real Win11
   backup to the lock screen through the crate.
5. **Windows guest boot** on a real capture (AHCI path). **DONE** (milestone 5
   above) — both write layers boot; Option A (`.avhdx`) is committed.
6. **GUI page:** "Virtualize" page (or an action on Mount) with the session
   list, boot/discard, RAM/CPU knobs — sticky-action-bar pattern. **NEXT.**
7. **Tests:** T1 config-matrix snapshots **DONE** (`phoenix-vm` unit tests:
   config synthesis + session create/resume/discard, no QEMU). Live boot stays
   an env-gated systest + manual checklist (needs QEMU + elevation on the box).

### Storage location

Everything a session creates — the write overlay, the per-session firmware,
and the on-demand serve junctions — lives on the **same volume as the `.phnx`**,
under `<image-drive>:\PhoenixSimulacra\{vm-sessions,vm-serve}`. Never the host
OS drive: a backup on `D:` must not fill `C:`. `simulacra-cli vm boot --vm-dir`
overrides the root; `vm list` scans every fixed volume (sessions follow their
image's drive, so there is no single root to look in). Only the growing overlay
consumes real space — the served `backup.vhdx` is a WinFsp file synthesized on
demand, not materialized.

**Planned (GUI):** let the user pick the **scratch drive** for these difference
files, rather than always defaulting to the image's volume. The image's drive is
a good default, but it isn't always the right one — the backup may live on a
slow or nearly-full external, while a fast local SSD is the better home for a
growing overlay. The engine already supports this (`--vm-dir` takes any root),
so the GUI needs a drive picker (showing free space per volume) that writes the
chosen root into settings, with "same drive as the image" as the default choice.

### Throughput: the qcow2 path is ~3× slower to boot (now critical-path)

End-to-end `simulacra-cli vm boot` on the real Win11 backup (2026-07-19):

| write path | boot → lock screen | teardown |
|---|---|---|
| `.avhdx` raw passthrough | ~4 min | **hangs indefinitely** (needs reboot) |
| **qcow2 (default)** | **~13 min** | **2 seconds, clean** |

So the pivot bought correctness at a real cost in speed. Measured mid-boot:
the **serve helper sits at ~50% of a single core** decompressing chunks while
the guest absorbs only ~2 MB/s of writes, with ~1.2 GB resident in the chunk
cache. The bottleneck is **single-threaded chunk decode in the serve process**,
not QEMU and not the qcow2 layer: a booting guest issues scattered reads and
each cache miss costs a whole 4 MiB chunk decompress.

That promotes the throughput work (**readahead + parallel chunk decode**) from
"nice to have" to **on the critical path** — 13 minutes to a lock screen is not
shippable. It is also the same engine work backup/restore/verify already want
(ROADMAP P3), so it pays off in both places.

### Still open before this is shippable

- ~~Serve-path resume proof.~~ **DONE 2026-07-19** for the qcow2 path
  (`vm_overlay_lifecycle`): after stopping the serve helper and re-spawning it
  at the same deterministic path, the qcow2 keeps its own writes *and* its
  backing chain reconnects to the re-served parent (GPT still readable through
  it). Guest-free, 12s, no attach.
- **A full stop→resume→continue with a real guest** still hasn't been run
  end-to-end (the guest-free proof covers the mechanism, not a Windows boot
  resuming its own prior state).
- **Crash/orphan sweep for VM sessions.** `sweep_serve_scratch` already scopes
  the sweep to `vm-serve` and never touches kept overlays (unit-tested). Still
  to do: reap an **orphaned serve helper process** whose boot process died
  (the helper self-exits on stdin EOF, so this is belt-and-braces).
- ~~Boot-it-or-mount-it interop~~ — **dropped** with the qcow2 pivot. Browsing a
  VM session's files needs a different route (`qemu-nbd`, or an offline
  `qemu-img` conversion) if we still want it.
- Readahead / parallel chunk decode (throughput), NVMe-on-stable-QEMU retest,
  MBR synthesis for BIOS captures, QEMU bundling — all as noted above.
- **Root-cause the `.avhdx` detach hang** if Option A is ever revived. The one
  unexplained data point is the early spike that detached cleanly (in-process
  serve, drop-based detach, light I/O, and a different attach storage-type
  parameter). Not on the critical path now.

## Guest access / login prep (planned feature)

When you boot someone's backup to recover data or triage a dead machine, you
often don't have their password — and Windows Hello PIN never works in a VM
anyway (TPM-sealed, confirmed in the boot smoke). So VM sessions should offer
**optional, opt-in guest-login prep**, applied to the throwaway overlay before
boot so the backing `.phnx` is never touched:

- **Enable the built-in Administrator account.** Offline `SYSTEM`/`SAM` hive
  edit on the mounted session overlay: clear the `ACB_DISABLED` bit on RID 500
  (and optionally blank its password). This is the same account state a
  clean-install recovery drops you into.
- **Bypass the password/PIN prompt** (the "Windows Login Unlocker"-style
  option): the robust, reversible way is to patch the on-disk password
  verification so any password is accepted for the session, rather than
  blanking hashes — because a blanked hash can cascade into losing DPAPI-
  protected secrets (saved credentials, EFS, browser data) that are sealed to
  the original password. Patching the check leaves those recoverable. Do this
  against the overlay only; the original backup keeps the real hashes.

Design constraints:

- **Overlay-only, always.** Every edit lands in the session's `.avhdx`/qcow2,
  never the `.phnx` — the immutability guarantee is the whole point of the tool.
- **Opt-in and clearly labelled** in the session UI (a checkbox at boot time,
  off by default), because it modifies the guest OS's security state.
- **BitLocker-gated.** A locked BitLocker OS volume can't be edited offline
  (raw ciphertext) — refuse, same as the writable mount. An unlocked-at-capture
  volume (plaintext image, like our test backup) is fine.
- **Implementation options to weigh:** shell out to `chntpw`/`reged`-style
  offline hive tooling vs. a small native SAM/`SYSTEM`-hive editor in Rust.
  The mount stack already gives us the volume — this rides on top of a
  session mount (the Option-A interop we just built).

This is a legitimate recovery capability for the machine's owner acting on
their own backup; it is deliberately session-scoped and reversible.

## Boot-from-ISO, next-boot-only (planned feature)

Let the user **attach an ISO and boot the VM from it on the next boot only** —
to drop into an emergency PE / rescue environment (WinPE, a Windows recovery
ISO, a live Linux) and inspect or repair the captured drive from the outside,
then fall back to booting the disk normally.

Why this is almost free in our model: we launch a fresh QEMU process on every
boot, so "next boot only" isn't persistent state to track and revert — it's
just a per-launch flag. A boot with the flag adds a read-only CD-ROM device
for the ISO and gives it boot priority; the *next* boot (without the flag)
simply doesn't, so it reverts automatically. No NVRAM varstore surgery, no
one-shot bookkeeping.

Sketch:

- Add the ISO as a read-only optical device (`-drive
  file=<iso>,media=cdrom,if=none,id=cd0` + an `ide-cd`/`scsi-cd`), and give it
  `bootindex=0` for that launch so firmware tries it before the disk. (QEMU's
  `-boot once=d` is the BIOS-path equivalent; with OVMF the per-device
  `bootindex` is the reliable lever.)
- The disk overlay is still attached, so the PE environment sees the guest's
  own drive — and any repair it makes lands in the throwaway session overlay,
  never the `.phnx`. This pairs naturally with the login-prep idea above (boot
  PE → fix the OS volume offline → next boot into the repaired system).
- UI: a "Boot from ISO next time" affordance on the session (pick a file);
  it's inherently one-shot, so nothing to un-set.

Notably this needs no new engine work — it's purely QEMU command-line
assembly in `VmConfig`/`BootSpec`, so it slots in wherever the boot options
UI lands.

## Open questions (want your input)

- Overlay: does session interop (mount the same session you boot) justify
  Option A's raw-passthrough complexity, or is qcow2-only (B) good enough?
  **RESOLVED 2026-07-18: Option A committed** — it booted Windows 11 over the
  `.avhdx` overlay and unlocks boot-it-or-mount-it interop; qcow2 kept as the
  portable fallback.
- Session UX: one session per backup, or multiple named sessions per backup?
  (Plan assumes one to start; the layout doesn't preclude more.)
- Login prep (above): ship both "enable Administrator" and "bypass password",
  or start with just enabling Administrator? Patch-the-check vs. blank-the-hash
  for the bypass (patch preferred, for DPAPI safety).
