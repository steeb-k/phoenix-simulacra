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

- [ ] From an elevated prompt, list disks: `simulacra-cli list-disks`.
- [ ] Back up the live OS disk (the running volume can't be locked, so the engine
      must escalate to a VSS shadow on its own — there is no flag):
      `simulacra-cli backup --disk 0 --partitions <sys parts> -o C:\temp\live.phnx`
- [ ] Confirm the log shows `volume is in use; reading a VSS shadow instead of the
      live volume` for the OS volume, and `VSS snapshot created`.
- [ ] Verify the backup: `simulacra-cli verify C:\temp\live.phnx` (full).
- [ ] Confirm `verify --quick` also passes and is fast.

## 2. Restore to a spare disk and boot

- [ ] Attach a spare disk (>= source used size).
- [ ] `simulacra-cli plan C:\temp\live.phnx --disk <spare> -o plan.toml`
- [ ] Review `plan.toml` sizes/offsets, then `simulacra-cli restore C:\temp\live.phnx --plan plan.toml`.
- [ ] Move the spare disk to a test machine (or set as boot device) and confirm Windows boots.
- [ ] If boot fails, note whether Startup Repair / `bcdedit` fixes it (expected for dissimilar hardware).

## 3. Live system-disk clone (VSS)

- [ ] `simulacra-cli clone --source-disk 0 --target-disk <spare> --verify`
- [ ] Confirm the destructive-action confirmation prompt appears and requires typed confirmation.
- [ ] After completion, `chkdsk <letter>: /scan` on each restored volume is clean.
- [ ] Boot the cloned disk; confirm Windows starts and data is intact.

## 4. Resize restores

- [ ] Restore an NTFS partition to a **larger** target; confirm the volume grew and mounts.
- [ ] Restore an NTFS partition to a **smaller** target (data must fit); confirm `chkdsk /F` is clean.

## 5. Mount

- [ ] `simulacra-cli mount C:\temp\live.phnx` (with WinFsp installed).
- [ ] Browse the mounted volume(s) in Explorer; open a few files; compare hashes to source.
- [ ] Unmount (Ctrl-C) and confirm the drive letters disappear cleanly.
- [ ] With WinFsp NOT installed, confirm the CLI/GUI shows a clear "install WinFsp" message.

## 6. ReFS on real hardware (if available)

> The automated `refs_family.rs` T2 suite covers VHD-backed ReFS end to end.
> This section is the real-media leg: a genuine ReFS partition captured from a
> physical disk and restored to another physical disk. Backing up is
> **read-only** on the source; only the restore target gets written.

- [ ] Identify the ReFS source partition and a scratch target disk with
      `Get-Volume` / `Get-Disk` (the target disk will be **wiped**).
- [ ] Put a few files with known hashes on the ReFS volume.
- [ ] Back it up: `simulacra-cli backup --disk <src> --partitions <refs part> -o C:\temp\refs.phnx`
      — confirm the log reports used-block capture (not raw) and `list` shows
      `fs=refs`, `used_bytes` far below the partition size.
- [ ] `simulacra-cli verify C:\temp\refs.phnx` passes.
- [ ] Restore to the scratch disk, same-or-larger size; confirm the restored
      volume mounts as ReFS (`Get-Volume` → `FileSystem : ReFS`) and the file
      hashes match.
- [ ] Restore into a **larger** slot; confirm the volume grew to fill it
      (FSCTL_EXTEND_VOLUME) and still reads clean.
- [ ] Attempt a **smaller** restore; confirm it is refused with the ReFS
      shrink message (this must fail — a success here is a bug).
- [ ] Mount the backup (`simulacra-cli mount`) and browse the ReFS volume in
      Explorer; compare a few file hashes.

## 7. 4Kn / USB media (if available)

> 4Kn is fully supported (backup, restore, clone, mount — see TESTING.md →
> "Tier 2-4Kn"), so any failure here is a finding, not a known limitation.
> Check what you actually have first — 512e (4096 physical / **512 logical**) is
> the common case:
> `Get-Disk | Select-Object Number, LogicalSectorSize, PhysicalSectorSize`

- [ ] Back up and restore a partition on a true 4Kn (`LogicalSectorSize = 4096`) disk.
- [ ] Repeat on a USB-attached disk.
