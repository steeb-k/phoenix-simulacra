# Tier-3 Live Smoke Checklist

These checks exercise behavior that can't be automated safely on a build
machine: backing up and cloning the **running** Windows system disk via VSS,
and booting a cloned disk. Run them manually before a release, on real hardware
or a spare disk you can afford to overwrite.

> See [../docs/TESTING.md](../docs/TESTING.md) for the full test-tier overview.
> Backup/restore/clone/resize round-trips on a real USB disk (MBR, NTFS/FAT32/
> exFAT) are now **automated** in `phoenix-systests/tests/real_disk.rs` — this
> checklist covers only what those can't (live system disk, boot, 4Kn).

> ⚠️ Every step here writes to real disks. Double-check disk numbers with
> `Get-Disk` before each destructive operation.

## 1. Live system-disk backup (VSS)

- [ ] From an elevated prompt, list disks: `simulacra list-disks`.
- [ ] Back up the live OS disk (the running volume can't be locked, so the engine
      must escalate to a VSS shadow on its own — there is no flag):
      `simulacra backup --disk 0 --partitions <sys parts> -o C:\temp\live.phnx`
- [ ] Confirm the log shows `volume is in use; reading a VSS shadow instead of the
      live volume` for the OS volume, and `VSS snapshot created`.
- [ ] Verify the backup: `simulacra verify C:\temp\live.phnx` (full).
- [ ] Confirm `verify --quick` also passes and is fast.

## 2. Restore to a spare disk and boot

- [ ] Attach a spare disk (>= source used size).
- [ ] `simulacra plan C:\temp\live.phnx --disk <spare> -o plan.toml`
- [ ] Review `plan.toml` sizes/offsets, then `simulacra restore C:\temp\live.phnx --plan plan.toml`.
- [ ] Move the spare disk to a test machine (or set as boot device) and confirm Windows boots.
- [ ] If boot fails, note whether Startup Repair / `bcdedit` fixes it (expected for dissimilar hardware).

## 3. Live system-disk clone (VSS)

- [ ] `simulacra clone --source-disk 0 --target-disk <spare> --verify`
- [ ] Confirm the destructive-action confirmation prompt appears and requires typed confirmation.
- [ ] After completion, `chkdsk <letter>: /scan` on each restored volume is clean.
- [ ] Boot the cloned disk; confirm Windows starts and data is intact.

## 4. Resize restores

- [ ] Restore an NTFS partition to a **larger** target; confirm the volume grew and mounts.
- [ ] Restore an NTFS partition to a **smaller** target (data must fit); confirm `chkdsk /F` is clean.

## 5. Mount

- [ ] `simulacra mount C:\temp\live.phnx` (with WinFsp installed).
- [ ] Browse the mounted volume(s) in Explorer; open a few files; compare hashes to source.
- [ ] Unmount (Ctrl-C) and confirm the drive letters disappear cleanly.
- [ ] With WinFsp NOT installed, confirm the CLI/GUI shows a clear "install WinFsp" message.

## 6. 4Kn / USB media (if available)

> ⚠️ **4Kn is currently BROKEN, not merely untested** (audited 2026-07-13). Capture
> fails on the first read with `ERROR_INVALID_PARAMETER` (Win32 87) — the FS parsers
> issue hardcoded 512-byte reads on a raw handle. Don't run this expecting a pass;
> see ROADMAP.md → "P1 — 4Kn media support". It reproduces on a 4Kn VHDX, so it does
> not need 4Kn hardware to fix (TESTING.md → "Tier 2-4Kn").
>
> Check which you actually have first — 512e (4096 physical / **512 logical**) is the
> common case and works fine:
> `Get-Disk | Select-Object Number, LogicalSectorSize, PhysicalSectorSize`

- [ ] Back up and restore a partition on a true 4Kn (`LogicalSectorSize = 4096`) disk — **expected to fail until P1 lands**.
- [ ] Repeat on a USB-attached disk.
