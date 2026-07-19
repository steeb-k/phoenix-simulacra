mod version;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::{default_plan_from_backup, RestorePlan};
use phoenix_restore::restore::{run_restore, verify_backup, RestoreOptions};
use tracing::info;

#[derive(Parser)]
// The CLI binary is `simulacra-cli` (the GUI owns the plain `simulacra` name).
// Pin the help/usage program name so `--help` matches what users type,
// independent of how the exe was invoked.
#[command(name = "simulacra-cli")]
#[command(about = "Phoenix Simulacra — Backup and Restore")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List physical disks and partitions
    ListDisks,
    /// Create a backup
    Backup {
        #[arg(long)]
        disk: u32,
        #[arg(long, value_delimiter = ',')]
        partitions: Vec<u32>,
        #[arg(short, long)]
        output: PathBuf,
        /// Skip the post-backup pass that re-reads the source and confirms the
        /// backup matches it (on by default; roughly doubles read time).
        #[arg(long, default_value_t = false)]
        no_verify: bool,
    },
    /// List contents of a .phnx backup
    List { backup: PathBuf },
    /// Verify backup integrity
    Verify {
        backup: PathBuf,
        /// Quick check: structural integrity + open-time checks (footer CRC,
        /// total-length/truncation, manifest & index hashes) + a sampled chunk
        /// hash. Omit for a full check that hashes every chunk.
        #[arg(long)]
        quick: bool,
    },
    /// Generate a default restore plan TOML
    Plan {
        backup: PathBuf,
        #[arg(long)]
        disk: u32,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Restore from backup using a plan file
    Restore {
        backup: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value_t = true)]
        verify: bool,
        /// Opt in to a 4Kn -> 512e sector-size conversion. Required when the
        /// backup is from a 4096-byte-logical-sector (4Kn) disk and the target
        /// uses 512-byte sectors: the filesystem boot sectors are rewritten so
        /// the restored volumes mount instead of coming up RAW. The result is a
        /// converted copy, not a byte-identical restore. Without this flag such
        /// a restore is refused; with matching sector sizes it does nothing.
        #[arg(long, default_value_t = false)]
        convert_sector_size: bool,
        /// After the restore, detect a Windows installation on the target disk
        /// and rebuild its boot environment (bcdboot/bootsect; drive-local,
        /// never touches this machine's firmware boot entries).
        #[arg(long, default_value_t = false)]
        repair_boot: bool,
    },
    /// Clone one disk directly onto another (no intermediate file)
    Clone {
        /// Source physical disk index (from `list-disks`)
        #[arg(long)]
        source_disk: u32,
        /// Target physical disk index — WILL BE ERASED
        #[arg(long)]
        target_disk: u32,
        /// Grow the largest NTFS partition to fill a larger target
        #[arg(long, default_value_t = false)]
        expand: bool,
        /// Read every written block back off the target and compare to source
        #[arg(long, default_value_t = false)]
        verify: bool,
        /// Deprecated and ignored. How a source is frozen is not a user choice:
        /// the engine locks each volume, escalates to a VSS shadow when the lock
        /// is refused, and aborts rather than read a lettered volume it cannot
        /// freeze. Accepted so existing scripts don't break.
        #[arg(long, default_value_t = false, hide = true)]
        no_vss: bool,
        /// Skip the interactive "type the target disk number" confirmation
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Opt in to a 4Kn -> 512e sector-size conversion. Required when the
        /// source disk uses 4096-byte logical sectors and the target uses
        /// 512-byte sectors: the filesystem boot sectors are rewritten so the
        /// cloned volumes mount instead of coming up RAW. The result is a
        /// converted copy, not a byte-identical clone. Without this flag such a
        /// clone is refused; with matching sector sizes it does nothing.
        #[arg(long, default_value_t = false)]
        convert_sector_size: bool,
        /// After the clone, detect a Windows installation on the target disk
        /// and rebuild its boot environment (bcdboot/bootsect; drive-local,
        /// never touches this machine's firmware boot entries).
        #[arg(long, default_value_t = false)]
        repair_boot: bool,
    },
    /// Detect Windows installations and repair a disk's boot environment
    /// (rebuild BCD/boot files with bcdboot, plus MBR boot code + active
    /// flag on legacy disks). Drive-local: this machine's firmware (NVRAM)
    /// boot entries are never modified. Without --disk, lists what was
    /// detected; without --apply, shows the plan without changing anything.
    BootRepair {
        /// Disk index carrying the Windows installation (see the no-argument
        /// listing or `list-disks`)
        #[arg(long)]
        disk: Option<u32>,
        /// Partition index of the Windows installation — only needed when the
        /// disk carries more than one
        #[arg(long)]
        partition: Option<u32>,
        /// Perform the repair (default is a dry run that prints the plan)
        #[arg(long, default_value_t = false)]
        apply: bool,
        /// Skip the confirmation prompt
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Mount a backup read-only so its files are browsable in Explorer. Runs
    /// in the foreground; press Enter to unmount.
    Mount {
        backup: PathBuf,
        /// Only expose these partition indices (comma-separated, as shown by
        /// `inspect`). Default: every volume gets a drive letter.
        #[arg(long, value_delimiter = ',')]
        partitions: Option<Vec<u32>>,
    },
    /// Dump every partition's stream header (extents + chunks) for forensic
    /// inspection. Useful when a manifest's extent table is suspected of
    /// containing garbage values (e.g. start_sectors near `u64::MAX`) — the
    /// restore path can't tell us anything about the bytes actually on disk
    /// in the .phnx, only what the validation rules trip on.
    Inspect {
        backup: PathBuf,
        /// Also dump per-chunk records (file_offset, compressed_len,
        /// uncompressed_len, extent_index, chunk_index). Off by default
        /// because a 21 GB partition has ~5400 chunks.
        #[arg(long, default_value_t = false)]
        full: bool,
    },
    /// Boot a backup as a QEMU virtual machine, or manage VM sessions. Guest
    /// writes go to a kept, resumable overlay; the backing .phnx is immutable.
    Vm {
        #[command(subcommand)]
        command: VmCommands,
    },
}

#[derive(Subcommand)]
enum VmCommands {
    /// Boot a backup as a VM (foreground; close the VM window to end). Creates
    /// a session on first boot and resumes it on later boots.
    Boot {
        backup: PathBuf,
        /// Write overlay: `qcow2` (default; QEMU-native, no VHD attach) or
        /// `avhdx` (EXPERIMENTAL raw passthrough — boots, but teardown can hang
        /// in the storage driver; see docs/VIRTUALIZATION.md).
        #[arg(long, default_value = "qcow2")]
        write: String,
        /// Guest RAM in MiB.
        #[arg(long, default_value_t = 6144)]
        mem: u64,
        /// Guest vCPUs.
        #[arg(long, default_value_t = 4)]
        cpus: u32,
        /// Boot with the guest's network cable plugged in. Off by default: a
        /// connected Windows guest gets a display driver pushed by Windows
        /// Update that blanks the screen on the next reboot. The NIC is
        /// always present either way — this is link state, and the GUI can
        /// plug it in later without a reboot.
        #[arg(long, default_value_t = false)]
        network: bool,
        /// Directory of the QEMU install (else PATH / default locations).
        #[arg(long)]
        qemu_dir: Option<PathBuf>,
        /// Working root for the VM's overlay + serve scratch. Default: a
        /// `PhoenixSimulacra` folder on the SAME drive as the backup, so
        /// nothing lands on the host OS drive.
        #[arg(long)]
        vm_dir: Option<PathBuf>,
        /// Discard the kept overlay and start from a clean first boot (recover
        /// a session that won't boot). Keeps the session identity.
        #[arg(long, default_value_t = false)]
        fresh: bool,
        /// Attach an ISO as a CD-ROM (e.g. a WinPE / rescue environment).
        #[arg(long)]
        iso: Option<PathBuf>,
        /// Attach a guest-tools / driver ISO (e.g. virtio-win) as a second,
        /// never-booted CD-ROM.
        #[arg(long)]
        drivers_iso: Option<PathBuf>,
        /// Share a folder next to the image with the guest over the host's SMB
        /// server (prints the `net use` command to run inside the guest).
        #[arg(long, default_value_t = false)]
        share: bool,
        /// Boot from the `--iso` this launch instead of the disk (one-shot: the
        /// next boot without this flag boots the disk normally).
        #[arg(long, default_value_t = false)]
        boot_iso: bool,
    },
    /// Gracefully power down a running VM (ACPI), instead of killing it.
    Stop { backup: PathBuf },
    /// List saved VM sessions.
    List,
    /// Delete a backup's VM session (overlay + firmware; the .phnx is untouched).
    Discard { backup: PathBuf },
}

fn main() -> anyhow::Result<()> {
    // If we were re-exec'd as an out-of-process WinFsp serve helper for a VM
    // boot, run that and exit — before any argument parsing, so the sentinel
    // isn't rejected by clap. (VM detach deadlocks against an in-process serve;
    // see phoenix_vm::serve_helper.)
    let raw_args: Vec<String> = std::env::args().collect();
    if let Some(res) = phoenix_vm::serve_helper::maybe_run(&raw_args) {
        if let Err(e) = res {
            eprintln!("vm serve helper failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // Match the GUI's default filter so a user running the CLI also sees
    // capture/restore progress events without having to set `RUST_LOG`.
    // The catch-all `warn` keeps third-party libs quiet.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "phoenix_core=info,phoenix_capture=info,phoenix_restore=info,\
                 phoenix_vss=info,phoenix_build=info,warn"
                    .into()
            }),
        )
        .init();

    phoenix_core::build_info::log_startup_banner(&crate::version::BUILD_INFO);

    let cli = Cli::parse();
    match cli.command {
        Commands::ListDisks => cmd_list_disks()?,
        Commands::Backup {
            disk,
            partitions,
            output,
            no_verify,
        } => {
            run_backup(BackupOptions {
                disk_index: disk,
                partition_indices: partitions,
                output,
                verify_after: !no_verify,
                verify_image: false,
                progress: None,
            })?;
        }
        Commands::List { backup } => cmd_list_backup(&backup)?,
        Commands::Verify { backup, quick } => {
            verify_backup(&backup, quick)?;
            if quick {
                info!(
                    "Quick verification OK (structure + open-time integrity + sampled chunk hashes)"
                );
            } else {
                info!("Full verification OK (every chunk hashed)");
            }
        }
        Commands::Plan {
            backup,
            disk,
            output,
        } => cmd_plan(&backup, disk, &output)?,
        Commands::Restore {
            backup,
            plan,
            verify,
            convert_sector_size,
            repair_boot,
        } => {
            let plan = RestorePlan::from_toml(&plan)?;
            let summary = run_restore(RestoreOptions {
                backup_path: backup,
                plan,
                verify_on_restore: verify,
                convert_sector_size,
                repair_boot,
                progress: None,
            })?;
            info!(
                partitions_restored = summary.partitions_restored,
                partitions_resized = summary.partitions_resized,
                partitions_converted = summary.partitions_converted,
                "Restore complete"
            );
            print_conversion_summary(
                summary.partitions_converted,
                summary.conversion_bootable,
                &summary.conversion_converted_names,
                &summary.conversion_unconverted_names,
            );
            if summary.restored_locked_bitlocker {
                println!(
                    "\nNote: a restored partition is a BitLocker-encrypted (locked) volume. \
                     It still requires the original BitLocker key/recovery password to unlock. \
                     If Windows shows it as unrecognized or decrypted, reboot or reconnect the \
                     disk and then unlock it."
                );
            }
            if let Some(status) = &summary.boot_repair {
                println!("\n{}", status.describe());
            }
        }
        Commands::Clone {
            source_disk,
            target_disk,
            expand,
            verify,
            no_vss,
            yes,
            convert_sector_size,
            repair_boot,
        } => {
            if no_vss {
                tracing::warn!(
                    "--no-vss is deprecated and ignored: the engine now always takes the                      strongest freeze each volume allows (lock, else VSS shadow, else abort                      for a lettered volume)"
                );
            }
            cmd_clone(
                source_disk,
                target_disk,
                expand,
                verify,
                yes,
                convert_sector_size,
                repair_boot,
            )?
        }
        Commands::BootRepair {
            disk,
            partition,
            apply,
            yes,
        } => cmd_boot_repair(disk, partition, apply, yes)?,
        Commands::Mount { backup, partitions } => cmd_mount(&backup, partitions.as_deref())?,
        Commands::Inspect { backup, full } => cmd_inspect(&backup, full)?,
        Commands::Vm { command } => cmd_vm(command)?,
    }
    Ok(())
}

fn cmd_mount(backup: &std::path::Path, partitions: Option<&[u32]>) -> anyhow::Result<()> {
    let scratch = phoenix_mount::mount_scratch_dir();
    // Reclaim anything a previous crashed run left behind before mounting anew.
    phoenix_mount::cleanup_leaked_mounts(&scratch);
    if phoenix_mount::ActiveMount::space_efficient() {
        println!("Mounting {} (read-only, on-demand)…", backup.display());
    } else {
        println!(
            "Materializing and attaching {} (read-only)…\n\
             note: this build lacks the `winfsp` feature, so it allocates a full-size temp \
             image. Build with --features winfsp for the zero-space mount.",
            backup.display()
        );
    }
    let session = phoenix_mount::ActiveMount::open_selected(backup, &scratch, partitions)?;
    println!(
        "Mounted a {:.1} GB virtual disk. Open Explorer to browse the volume(s).",
        session.disk_size() as f64 / 1e9
    );
    for v in session.volumes() {
        match v.drive_letter {
            Some(l) => println!("  partition {} -> {l}:", v.partition_index),
            None => println!("  partition {} -> no mountable volume", v.partition_index),
        }
    }
    println!("Press Enter to unmount.");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    drop(session);
    println!("Unmounted.");
    Ok(())
}

fn cmd_vm(command: VmCommands) -> anyhow::Result<()> {
    use phoenix_vm::{HostOptions, Qemu, SessionManager, WriteLayer};

    match command {
        VmCommands::Boot {
            backup,
            write,
            mem,
            cpus,
            network,
            qemu_dir,
            vm_dir,
            fresh,
            iso,
            boot_iso,
            drivers_iso,
            share,
        } => {
            let write_layer = match write.to_ascii_lowercase().as_str() {
                "avhdx" => {
                    eprintln!(
                        "warning: --write avhdx is EXPERIMENTAL. The guest boots, but detaching \
                         the overlay at shutdown can hang in the Windows storage driver \
                         (leaving a stuck attach). Use the default qcow2 unless you are \
                         specifically testing this path."
                    );
                    WriteLayer::Avhdx
                }
                "qcow2" => WriteLayer::Qcow2,
                other => anyhow::bail!("unknown --write value {other:?} (use qcow2 or avhdx)"),
            };
            let qemu = Qemu::discover(qemu_dir.as_deref())?;
            println!("Using QEMU: {} ({})", qemu.system.display(), qemu.version);

            let host = HostOptions {
                memory_mib: mem,
                smp: cpus,
                network,
                // Cap the guest display so the QEMU window (chrome included)
                // fits the monitor — PE otherwise picks a huge video mode.
                max_resolution: phoenix_vm::usable_guest_resolution(),
                // Clipboard sharing only where this build supports it (11.1+).
                clipboard_agent: qemu.gtk_clipboard,
                ..HostOptions::default()
            };
            // Everything (session overlay + serve scratch) stays on the image's
            // drive by default, so a backup on D: never fills the OS drive.
            let vm_root = vm_dir.unwrap_or_else(|| phoenix_vm::vm_root_for_backup(&backup));
            let sessions = SessionManager::new(vm_root.join("vm-sessions"));
            let scratch = vm_root.join("vm-serve");
            println!("VM working dir: {}", vm_root.display());
            // Session-aware sweep: reclaims stale serve junctions under
            // vm-serve, never touches kept overlays under vm-sessions.
            phoenix_vm::sweep_serve_scratch(&vm_root);

            println!("Booting {} — close the VM window to end.", backup.display());
            #[cfg(feature = "winfsp")]
            {
                if let Some(iso) = &iso {
                    anyhow::ensure!(iso.exists(), "ISO not found: {}", iso.display());
                }
                if let Some(iso) = &drivers_iso {
                    anyhow::ensure!(iso.exists(), "drivers ISO not found: {}", iso.display());
                }
                // The helper disk rides along on every boot, share or not: it
                // also carries InstallGuestDrivers.cmd, which matters most
                // when the guest has no network to fetch anything with.
                let img = vm_root.join("share-helper.img");
                let mut helper_disk = None;
                match phoenix_vm::share::build_helper_disk(&img) {
                    Ok(()) => helper_disk = Some(img),
                    Err(e) => eprintln!("warning: helper disk build failed: {e:#}"),
                }
                if share {
                    let dir = phoenix_vm::share::share_dir_for_backup(&backup);
                    phoenix_vm::share::ensure_share(&dir)?;
                    println!("Sharing {} with the guest.", dir.display());
                    println!(
                        "In the guest: open the SIMULACRA drive and run MapShare.cmd, or run:  {}",
                        phoenix_vm::share::guest_mount_command()
                    );
                }
                let outcome = phoenix_vm::boot::boot(
                    &backup,
                    &host,
                    write_layer,
                    fresh,
                    iso.as_deref(),
                    boot_iso,
                    drivers_iso.as_deref(),
                    helper_disk.as_deref(),
                    &qemu,
                    &sessions,
                    &scratch,
                );
                // The share exists for the guest; drop it even when the boot
                // errored (the folder and its contents stay).
                if share {
                    phoenix_vm::share::remove_share();
                }
                let outcome = outcome?;
                if outcome.resumed {
                    println!(
                        "Resumed existing session{}.",
                        if outcome.resumed_dirty {
                            " (was left DIRTY by a prior crash — use --fresh if it won't boot)"
                        } else {
                            ""
                        }
                    );
                }
                println!(
                    "VM exited ({}). Session overlay holds {:.1} GB of guest writes; backup {}.",
                    if outcome.exit_ok { "clean" } else { "non-zero" },
                    outcome.overlay_bytes as f64 / 1e9,
                    if outcome.backup_intact {
                        "still verifies"
                    } else {
                        "FAILED verification"
                    }
                );
                // QEMU's output goes to the session's qemu.log now (not the
                // console) — quote it when the exit was an error.
                if let Some(tail) = outcome.qemu_log_tail.as_deref().filter(|t| !t.is_empty()) {
                    eprintln!("QEMU output:\n{tail}");
                }
            }
            #[cfg(not(feature = "winfsp"))]
            {
                let _ = (
                    &host,
                    write_layer,
                    fresh,
                    &iso,
                    boot_iso,
                    &drivers_iso,
                    share,
                    &qemu,
                    &sessions,
                    &scratch,
                );
                anyhow::bail!(
                    "this build lacks the `winfsp` feature required to boot a VM; \
                     rebuild with --features winfsp"
                );
            }
        }
        VmCommands::Stop { backup } => {
            let reader = phoenix_core::container::PhnxReader::open(&backup)?;
            let id = reader.header.backup_id;
            drop(reader);
            let session = SessionManager::for_backup(&backup).open_or_create(
                id,
                &backup,
                WriteLayer::Qcow2,
                phoenix_vm::now_rfc3339(),
            )?;
            let port_file = session.qmp_port_file();
            let port: u16 = std::fs::read_to_string(&port_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no running VM for {} (no QMP port recorded)",
                        backup.display()
                    )
                })?;
            phoenix_vm::qmp::system_powerdown(port)?;
            println!("Sent ACPI power-down to the VM; it will shut down gracefully.");
        }
        VmCommands::List => {
            // Sessions live on their image's drive, so scan every volume.
            let list = SessionManager::list_all();
            if list.is_empty() {
                println!("No VM sessions.");
            } else {
                for m in list {
                    println!(
                        "{}  {:>5}  {}  booted={}  {}",
                        m.backup_id,
                        match m.write_layer {
                            WriteLayer::Avhdx => "avhdx",
                            WriteLayer::Qcow2 => "qcow2",
                        },
                        m.backup_path,
                        m.last_booted_at.as_deref().unwrap_or("never"),
                        if m.clean_shutdown { "clean" } else { "DIRTY" },
                    );
                }
            }
        }
        VmCommands::Discard { backup } => {
            let reader = phoenix_core::container::PhnxReader::open(&backup)?;
            let id = reader.header.backup_id;
            drop(reader);
            // The session lives on the backup's own drive.
            SessionManager::for_backup(&backup).discard(&id)?;
            println!("Discarded VM session for {}.", backup.display());
        }
    }
    Ok(())
}

fn cmd_boot_repair(
    disk_index: Option<u32>,
    partition: Option<u32>,
    apply: bool,
    yes: bool,
) -> anyhow::Result<()> {
    use phoenix_restore::bootrepair::{
        detect_windows_installs, execute_boot_repair, plan_boot_repair, system_disk_index,
    };

    let mut disks = enumerate_disks()?;
    for d in &mut disks {
        for p in &mut d.partitions {
            phoenix_core::disk::refine_partition_fs(p);
        }
    }
    let installs = detect_windows_installs(&disks);
    if installs.is_empty() {
        println!("No Windows installations detected on any disk.");
        return Ok(());
    }

    let Some(disk_index) = disk_index else {
        println!("Windows installations detected:");
        for i in &installs {
            println!(
                "  --disk {} --partition {}  ->  {}",
                i.disk_index,
                i.partition_index,
                i.display()
            );
        }
        println!("\nRe-run with --disk (and --partition if listed twice for one disk).");
        return Ok(());
    };

    let on_disk: Vec<_> = installs
        .iter()
        .filter(|i| i.disk_index == disk_index)
        .collect();
    let install = match (on_disk.len(), partition) {
        (0, _) => anyhow::bail!("no Windows installation detected on disk {disk_index}"),
        (1, None) => on_disk[0],
        (_, None) => anyhow::bail!(
            "disk {disk_index} carries {} Windows installations; pick one with --partition",
            on_disk.len()
        ),
        (_, Some(p)) => on_disk
            .iter()
            .find(|i| i.partition_index == p)
            .ok_or_else(|| {
                anyhow::anyhow!("no Windows installation on disk {disk_index} partition {p}")
            })?,
    };

    let disk = disks
        .iter()
        .find(|d| d.index == disk_index)
        .expect("install came from this disk list");
    let plan = plan_boot_repair(disk, install)?;

    println!("Boot repair plan for {}:", install.display());
    for a in &plan.actions {
        println!("  • {a}");
    }
    if system_disk_index(&disks) == Some(disk_index) {
        println!(
            "\nWARNING: disk {disk_index} is the disk this machine is currently running from — \
             this rewrites the live system's boot files."
        );
    }
    if !apply {
        println!("\nDry run — re-run with --apply to perform the repair.");
        return Ok(());
    }
    if !yes {
        use std::io::Write;
        print!("Type the disk number ({disk_index}) to proceed: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != disk_index.to_string() {
            anyhow::bail!("confirmation did not match; aborting");
        }
    }

    let report = execute_boot_repair(disk, &plan)?;
    println!("\nBoot repair completed:");
    for a in &report.actions {
        println!("  • {a}");
    }
    Ok(())
}

fn cmd_clone(
    source_disk: u32,
    target_disk: u32,
    expand: bool,
    verify: bool,
    yes: bool,
    convert_sector_size: bool,
    repair_boot: bool,
) -> anyhow::Result<()> {
    use phoenix_clone::{run_clone, CloneOptions, ClonePlan, CloneVerify};

    let mut disks = enumerate_disks()?;
    for d in &mut disks {
        for p in &mut d.partitions {
            phoenix_core::disk::refine_partition_fs(p);
        }
    }
    let source = disks
        .iter()
        .find(|d| d.index == source_disk)
        .ok_or_else(|| anyhow::anyhow!("source disk {source_disk} not found"))?
        .clone();
    let target = disks
        .iter()
        .find(|d| d.index == target_disk)
        .ok_or_else(|| anyhow::anyhow!("target disk {target_disk} not found"))?
        .clone();

    let plan = if expand {
        ClonePlan::expand_to_fill(&source, &target)
    } else {
        ClonePlan::identity(&source)
    };
    plan.validate(&source, &target)?;

    println!(
        "About to CLONE disk {} ({}, {:.1} GB) onto disk {} ({}, {:.1} GB).",
        source.index,
        source.model.as_deref().unwrap_or("disk"),
        source.size_bytes as f64 / 1e9,
        target.index,
        target.model.as_deref().unwrap_or("disk"),
        target.size_bytes as f64 / 1e9,
    );
    println!("ALL DATA ON DISK {target_disk} WILL BE PERMANENTLY ERASED.");

    if !yes {
        use std::io::Write;
        print!("Type the target disk number ({target_disk}) to proceed: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != target_disk.to_string() {
            anyhow::bail!("confirmation did not match; aborting");
        }
    }

    let summary = run_clone(CloneOptions {
        source_disk_index: source_disk,
        target_disk_index: target_disk,
        plan,
        verify: if verify {
            CloneVerify::ReadBack
        } else {
            CloneVerify::None
        },
        convert_sector_size,
        repair_boot,
        progress: None,
    })?;
    info!(
        partitions_cloned = summary.partitions_cloned,
        partitions_resized = summary.partitions_resized,
        partitions_converted = summary.partitions_converted,
        "Clone complete"
    );
    print_conversion_summary(
        summary.partitions_converted,
        summary.conversion_bootable,
        &summary.conversion_converted_names,
        &summary.conversion_unconverted_names,
    );
    if let Some(status) = &summary.boot_repair {
        println!("\n{}", status.describe());
    }
    Ok(())
}

/// Post-op summary for a 4Kn -> 512e conversion (Alert G). No-op when no
/// conversion ran (`bootable` is `None`).
fn print_conversion_summary(
    converted: u32,
    bootable: Option<bool>,
    converted_names: &[String],
    unconverted_names: &[String],
) {
    let Some(bootable) = bootable else {
        return;
    };
    println!("\nSector-size conversion (4Kn -> 512e): {converted} partition(s) converted.");
    for n in converted_names {
        println!("  converted: {n}");
    }
    for n in unconverted_names {
        println!("  left unconverted (exFAT/encrypted — restored but unreadable on 512e): {n}");
    }
    if bootable {
        println!(
            "  The EFI System Partition was converted — the disk should be bootable (firmware \
             boot is not verified here)."
        );
    } else {
        println!(
            "  The EFI System Partition was NOT converted — the disk holds its data but will not \
             boot."
        );
    }
}

fn cmd_list_disks() -> anyhow::Result<()> {
    let mut disks = enumerate_disks()?;
    for disk in &mut disks {
        for part in &mut disk.partitions {
            phoenix_core::disk::refine_partition_fs(part);
        }
        // Flag 4Kn media explicitly — it's the signal that a restore/clone onto
        // 512e media needs --convert-sector-size (and vice-versa is refused).
        let sector_note = match disk.sector_size {
            4096 => " 4Kn",
            512 => " 512e",
            _ => "",
        };
        println!(
            "Disk {}: {} ({:.2} GB, {}, {}-byte sectors{})",
            disk.index,
            disk.path,
            disk.size_bytes as f64 / 1e9,
            if disk.is_gpt { "GPT" } else { "MBR" },
            disk.sector_size,
            sector_note,
        );
        for p in &disk.partitions {
            let bitlocker = match p.bitlocker {
                phoenix_core::disk::BitlockerState::None => String::new(),
                phoenix_core::disk::BitlockerState::Unlocked => {
                    ", bitlocker=UNLOCKED (normal plaintext backup)".to_string()
                }
                phoenix_core::disk::BitlockerState::Locked => {
                    ", bitlocker=LOCKED (backup would be raw ciphertext)".to_string()
                }
            };
            println!(
                "  [{}] {} — {} bytes, fs={:?}, mode={:?}{}{}",
                p.index,
                p.name,
                p.size_bytes,
                p.fs_kind,
                p.capture_mode,
                bitlocker,
                p.volume_path
                    .as_ref()
                    .map(|v| format!(", vol={v}"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn cmd_list_backup(path: &Path) -> anyhow::Result<()> {
    let reader = PhnxReader::open(path)?;
    println!("Backup: {}", path.display());
    println!("  ID: {}", reader.header.backup_id);
    println!("  Created: {}", reader.header.timestamp);
    println!("  Host: {}", reader.manifest.hostname);
    println!("  Disk style: {}", reader.manifest.disk.style);
    for e in &reader.index {
        let pm = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.index == e.index);
        let bitlocker = match pm.and_then(|p| p.bitlocker.as_deref()) {
            Some("locked") => " [BitLocker ciphertext — needs original key to unlock]",
            Some("unlocked") => " [was BitLocker; image is plaintext]",
            _ => "",
        };
        println!(
            "  Partition [{}] {} — {} / {} bytes used, {} chunks{}",
            e.index,
            e.name,
            e.original_size,
            e.used_bytes,
            pm.map(|p| p.chunks.len()).unwrap_or(0),
            bitlocker
        );
    }
    Ok(())
}

fn cmd_plan(backup: &Path, disk_index: u32, output: &PathBuf) -> anyhow::Result<()> {
    let reader = PhnxReader::open(backup)?;
    let disks = enumerate_disks()?;
    let target = disks
        .iter()
        .find(|d| d.index == disk_index)
        .ok_or_else(|| anyhow::anyhow!("disk not found"))?;
    let plan = default_plan_from_backup(
        &backup.to_string_lossy(),
        &reader,
        disk_index,
        target.size_bytes,
    );
    std::fs::write(output, plan.to_toml()?)?;
    println!("Wrote restore plan to {}", output.display());
    Ok(())
}

/// Dump the on-disk structure of a `.phnx` so we can see what the
/// capture path actually wrote. The high-level [`cmd_list_backup`]
/// stops at the per-partition manifest summary; this goes one level
/// deeper and reads each partition's stream header (extent table +
/// chunk index). Used when a manifest is suspected of containing
/// garbage values — the validate-extents-fit pre-flight will tell us
/// "extent reaches sector N", but only this tool can say *which*
/// extent and what its raw bytes look like.
fn cmd_inspect(path: &Path, full: bool) -> anyhow::Result<()> {
    let mut reader = PhnxReader::open(path)?;
    println!("Backup: {}", path.display());
    println!("  ID:         {}", reader.header.backup_id);
    println!("  Format ver: {}", reader.header.version);
    println!("  Flags:      {} (bit 0 = GPT)", reader.header.flags);
    println!("  Timestamp:  {}", reader.header.timestamp);
    println!("  Host:       {}", reader.manifest.hostname);
    println!(
        "  Disk:       style={}, sector_size={}, signature={:#x}",
        reader.manifest.disk.style, reader.manifest.disk.sector_size, reader.header.disk_signature,
    );
    println!("  Partitions: {}", reader.index.len());

    let entries: Vec<_> = reader.index.clone();
    for entry in &entries {
        println!();
        println!("Partition {} — \"{}\"", entry.index, entry.name,);
        println!(
            "  fs={:?}  capture={:?}  sector_size={}  original={} bytes  used={} bytes",
            entry.fs_kind,
            entry.capture_mode,
            entry.sector_size,
            entry.original_size,
            entry.used_bytes,
        );
        println!(
            "  stream_offset={}  stream_length={}",
            entry.stream_offset, entry.stream_length,
        );
        let stream = match reader.read_stream_header(entry) {
            Ok(s) => s,
            Err(e) => {
                println!("  ! failed to read stream header: {e}");
                continue;
            }
        };
        println!(
            "  bytes_per_cluster={}  extents={}  chunks={}",
            stream.bytes_per_cluster,
            stream.extents.len(),
            stream.chunks.len(),
        );
        let sector_size = entry.sector_size.max(1) as u64;
        // Sentinels for "this number is suspiciously close to u64::MAX",
        // which is the symptom that triggered this whole investigation.
        // Anything within 1 PiB of u64::MAX is impossible for any real
        // storage device on Earth and almost certainly garbage.
        let suspicious_threshold: u64 = u64::MAX - (1u64 << 50);
        for (idx, ext) in stream.extents.iter().enumerate() {
            let last_sector = ext.start_sector.saturating_add(ext.sector_count);
            let last_byte = last_sector.saturating_mul(sector_size);
            let flag = if ext.start_sector > suspicious_threshold
                || ext.sector_count > suspicious_threshold
                || last_sector > suspicious_threshold
            {
                "  [SUSPICIOUS — values near u64::MAX]"
            } else {
                ""
            };
            println!(
                "    extent[{idx:5}] start={:>20}  count={:>20}  last_sector={:>20}  last_byte~{:>20}{flag}",
                ext.start_sector, ext.sector_count, last_sector, last_byte,
            );
        }
        if full {
            for (idx, chunk) in stream.chunks.iter().enumerate() {
                println!(
                    "    chunk[{idx:6}] file_offset={:>14}  compressed_len={:>10}  uncompressed_len={:>10}  extent_index={:>5}  chunk_index={:>5}",
                    chunk.file_offset,
                    chunk.compressed_len,
                    chunk.uncompressed_len,
                    chunk.extent_index,
                    chunk.chunk_index,
                );
            }
        }
    }
    Ok(())
}
