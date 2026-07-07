mod version;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::{default_plan_from_backup, RestorePlan};
use phoenix_restore::restore::{run_restore, verify_backup, RestoreOptions};
use tracing::info;

#[derive(Parser)]
#[command(name = "carbon-phoenix")]
#[command(about = "Carbon Phoenix — Backup and Restore")]
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
        #[arg(long, default_value_t = false)]
        vss: bool,
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
    },
    /// Clone one disk directly onto another (no intermediate file)
    Clone {
        /// Source physical disk index (from `list-disks`)
        #[arg(long)]
        source_disk: u32,
        /// Target physical disk index — WILL BE ERASED
        #[arg(long)]
        target_disk: u32,
        /// Grow the last NTFS partition to fill a larger target
        #[arg(long, default_value_t = false)]
        expand: bool,
        /// Read every written block back off the target and compare to source
        #[arg(long, default_value_t = false)]
        verify: bool,
        /// Do NOT use a VSS snapshot for live source volumes
        #[arg(long, default_value_t = false)]
        no_vss: bool,
        /// Skip the interactive "type the target disk number" confirmation
        #[arg(long, default_value_t = false)]
        yes: bool,
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
}

fn main() -> anyhow::Result<()> {
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
            vss,
        } => {
            run_backup(BackupOptions {
                disk_index: disk,
                partition_indices: partitions,
                output,
                use_vss: vss,
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
        } => {
            let plan = RestorePlan::from_toml(&plan)?;
            let summary = run_restore(RestoreOptions {
                backup_path: backup,
                plan,
                verify_on_restore: verify,
                progress: None,
            })?;
            info!(
                partitions_restored = summary.partitions_restored,
                partitions_resized = summary.partitions_resized,
                "Restore complete"
            );
        }
        Commands::Clone {
            source_disk,
            target_disk,
            expand,
            verify,
            no_vss,
            yes,
        } => cmd_clone(source_disk, target_disk, expand, verify, !no_vss, yes)?,
        Commands::Inspect { backup, full } => cmd_inspect(&backup, full)?,
    }
    Ok(())
}

fn cmd_clone(
    source_disk: u32,
    target_disk: u32,
    expand: bool,
    verify: bool,
    use_vss: bool,
    yes: bool,
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
        use_vss,
        progress: None,
    })?;
    info!(
        partitions_cloned = summary.partitions_cloned,
        partitions_resized = summary.partitions_resized,
        "Clone complete"
    );
    Ok(())
}

fn cmd_list_disks() -> anyhow::Result<()> {
    let mut disks = enumerate_disks()?;
    for disk in &mut disks {
        for part in &mut disk.partitions {
            phoenix_core::disk::refine_partition_fs(part);
        }
        println!(
            "Disk {}: {} ({:.2} GB, {})",
            disk.index,
            disk.path,
            disk.size_bytes as f64 / 1e9,
            if disk.is_gpt { "GPT" } else { "MBR" }
        );
        for p in &disk.partitions {
            println!(
                "  [{}] {} — {} bytes, fs={:?}, mode={:?}{}",
                p.index,
                p.name,
                p.size_bytes,
                p.fs_kind,
                p.capture_mode,
                p.volume_path
                    .as_ref()
                    .map(|v| format!(", vol={v}"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn cmd_list_backup(path: &PathBuf) -> anyhow::Result<()> {
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
        println!(
            "  Partition [{}] {} — {} / {} bytes used, {} chunks",
            e.index,
            e.name,
            e.original_size,
            e.used_bytes,
            pm.map(|p| p.chunks.len()).unwrap_or(0)
        );
    }
    Ok(())
}

fn cmd_plan(backup: &PathBuf, disk_index: u32, output: &PathBuf) -> anyhow::Result<()> {
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
fn cmd_inspect(path: &PathBuf, full: bool) -> anyhow::Result<()> {
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
