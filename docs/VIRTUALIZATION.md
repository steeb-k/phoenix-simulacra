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

### Write path (the main open decision)

Constraint discovered up front: **QEMU's `vhdx` block driver does not support
differencing (parented) VHDX images.** So QEMU cannot do its I/O directly
against a `child-*.avhdx`. Two viable shapes:

**Option A — raw physical-drive passthrough over the existing `.avhdx` child
(recommended).** Reuse the shipped overlay *literally*:

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

Plan: **spike both in milestone 2 and let the results pick.** A is the default
if raw-passthrough proves stable, because session interop (boot it *or* mount
it) is a genuinely differentiating feature; B is the fallback and the eventual
non-Windows story either way.

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
Bundling QEMU in the installer stays an open question — it's ~150–300 MB and
GPL, fine to redistribute but a big installer jump; revisit after the
exploration proves value.

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

## Milestones

1. **Plan + branch** (this document).
2. **Write-path spike (decision point):** hand-drive both Option A (offline
   attach + `\\.\PhysicalDriveN` raw) and Option B (qcow2 over served VHDX)
   against a small Linux backup; measure stability and I/O sanity; pick.
3. **`phoenix-vm` crate:** QEMU discovery, `VmConfig` synthesis from the
   manifest (unit-tested matrix), command-line builder, process
   lifecycle + teardown.
4. **Sessions:** persistent overlay + `session.json`, resume/discard,
   orphan sweep. CLI: `phoenix vm boot|list|discard`.
5. **Windows guest boot** on a real capture (AHCI path), working the 0x7B
   list. This is where the exploration proves or disproves itself.
6. **GUI page:** "Virtualize" page (or an action on Mount) with the session
   list, boot/discard, RAM/CPU knobs — sticky-action-bar pattern.
7. **Tests:** T1 = config-matrix snapshots (no QEMU). T2-style boot smoke
   needs QEMU on the runner box and is heavy — likely a staged script plus a
   manual checklist entry, like the live-VSS checklist, rather than cargo tests.

## Open questions (want your input)

- Overlay: does session interop (mount the same session you boot) justify
  Option A's raw-passthrough complexity, or is qcow2-only (B) good enough?
  (Spike will inform, but the *product* preference matters too.)
- Session UX: one session per backup, or multiple named sessions per backup?
  (Plan assumes one to start; the layout doesn't preclude more.)
- Bundle QEMU in the installer eventually, or keep it detect-and-link forever?
