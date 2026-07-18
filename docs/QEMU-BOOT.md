# Booting backups in QEMU (Phase 2 plan)

Status: **planned / exploratory**. This builds directly on the writable
copy-on-write overlay shipped in 0.3.0 (see the mount page's "Enable Write
Access"); it is not yet implemented.

## Goal

Boot a `.phnx` backup as a live virtual machine — spin the captured system up in
QEMU to check "does this backup actually boot?", recover a file from a running
OS, or triage a machine without touching real hardware — **without ever
modifying the backup and without materializing it to disk**.

## The idea in one line

`.phnx` → a read-only on-demand disk-image server → QEMU booting a **read-only
base** with a **qcow2 overlay** for all guest writes.

This is the same copy-on-write philosophy as the Windows writable mount, just
one layer out: there, Windows differencing VHDx does the CoW into a child
`.avhdx`; here, QEMU's qcow2 overlay does the CoW over a read-only base. The
backup stays immutable and keeps verifying in both worlds.

## Why this shape

The mount stack already synthesizes a whole disk image on demand from the
compressed backup and BLAKE3-verifies every chunk it serves
(`phoenix-mount`: `SyntheticVhd::read_at` → `ChunkStore`). We want QEMU to read
*that*, not a 500 GB temp file. Two ways to feed it:

1. **NBD server (recommended).** Stand up a small read-only NBD server whose
   read handler calls straight into `SyntheticVhd::read_at`. QEMU speaks NBD
   natively (`-drive file=nbd:...` / `-blockdev`), so:
   - no materialization — reads are answered on demand from the `.phnx`,
   - **OS-agnostic** — an NBD server has no WinFsp/Windows dependency, so this
     path can also run off-Windows,
   - reuses the existing hash-verified synthesis unchanged.
   QEMU layers a `qcow2` overlay over the NBD export for every guest write, so
   the backup is read-only and the writes are throwaway (or kept as a named
   overlay if the user wants to resume a session).

2. **Serve the existing synthetic VHDX to QEMU directly** (Windows-only):
   `-drive file=<winfsp-served>.vhdx,readonly=on` + a qcow2 overlay. Simpler to
   reach from the current mount code, but Windows-only and tied to WinFsp.
   Prefer NBD; keep this as a fallback.

## Components to build

- **`phoenix-nbd` (new crate):** a minimal NBD server exposing one read-only
  export backed by `SyntheticVhd`. Handles `NBD_CMD_READ` (delegating to
  `read_at`), advertises the disk size, refuses writes. Bind to `127.0.0.1` on
  an ephemeral port.
- **A QEMU launcher:** builds the QEMU command line from the backup's manifest —
  firmware, machine type, disk/overlay wiring — creates the qcow2 overlay
  (`qemu-img create -f qcow2 -F raw -b nbd:... overlay.qcow2`), starts QEMU,
  and tears the NBD server + overlay down on exit (overlay is ephemeral by
  default, like the mount's differencing child).
- **GUI/CLI entry point:** a "Boot in VM" action on the Mount page (or a CLI
  `boot` subcommand). QEMU itself is an external dependency the app locates or
  the user points at — do **not** bundle it initially.

## Boot-time challenges (the hard part — flagged, not solved)

Booting a *physical* capture on *virtual* hardware is where this gets real:

- **Storage-driver mismatch — the big one.** A physical Windows install booting
  on QEMU's virtual disk controller commonly bugchecks `INACCESSIBLE_BOOT_DEVICE`
  (0x7B): the boot-critical storage driver for the emulated controller isn't in
  the captured image. Mitigations, roughly in order of effort: present an
  IDE/AHCI controller Windows has inbox drivers for; pre-inject virtio-blk /
  virtio-scsi drivers; or a one-time "prep" pass. Linux guests are far more
  forgiving.
- **Firmware match.** UEFI/GPT vs BIOS/MBR must match the capture. We synthesize
  the GPT (`phoenix-mount::gpt`) and know the disk style from the manifest, so
  the launcher can auto-select OVMF (UEFI) vs SeaBIOS (BIOS).
- **BitLocker + TPM + Secure Boot.** A TPM-sealed BitLocker volume will not
  unseal on QEMU without an emulated TPM holding the same key; Secure Boot state
  differs from the physical machine. A locked BitLocker partition is effectively
  non-bootable without the key (and is already refused for writable mounts —
  same raw-ciphertext reason).
- **Activation / PnP.** Windows may demand reactivation after the hardware
  change and re-enumerate drivers on first boot.
- **Multi-disk machines.** One `.phnx` is one disk; a system that spans several
  disks won't fully boot from a single backup.

## Relationship to the shipped overlay

| | Windows writable mount (shipped) | QEMU boot (this plan) |
|---|---|---|
| Read path | `SyntheticVhd` via WinFsp | `SyntheticVhd` via NBD |
| CoW writes | differencing `.avhdx` child | `qcow2` overlay |
| Backup touched | never | never |
| Footprint | diff only | overlay only |
| Ephemeral | child deleted on unmount | overlay deleted on exit |

The synthesis + verification core is shared; Phase 2 mostly adds a second
front-end (NBD) and the VM orchestration around it.

## Suggested milestones

1. **NBD spike:** `phoenix-nbd` serving a known-good **Linux** backup; boot it in
   QEMU with a qcow2 overlay; confirm the export stays read-only and the `.phnx`
   still verifies afterward. Linux sidesteps the 0x7B driver problem, so this
   proves the plumbing.
2. **Windows boot:** the same for a Windows capture, working the storage-driver
   mismatch (start with AHCI/IDE, then virtio + driver injection).
3. **Firmware auto-select** from the manifest (OVMF/SeaBIOS).
4. **GUI "Boot in VM"** action + ephemeral-overlay lifecycle, mirroring the
   mount's teardown/cleanup discipline.

## Open questions

- Bundle QEMU, or require the user to install it and point the app at it?
- Persist a session (named qcow2 overlay) or always ephemeral?
- How far to go on Windows driver injection vs. documenting "boots best from
  captures already taken on virtual/AHCI hardware".
