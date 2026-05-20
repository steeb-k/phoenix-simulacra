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
    List {
        backup: PathBuf,
    },
    /// Verify backup integrity
    Verify {
        backup: PathBuf,
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
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

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
            })?;
        }
        Commands::List { backup } => cmd_list_backup(&backup)?,
        Commands::Verify { backup, quick } => {
            verify_backup(&backup, quick)?;
            info!("Verification OK");
        }
        Commands::Plan {
            backup,
            disk,
            output,
        } => cmd_plan(&backup, disk, &output)?,
        Commands::Restore { backup, plan, verify } => {
            let plan = RestorePlan::from_toml(&plan)?;
            run_restore(RestoreOptions {
                backup_path: backup,
                plan,
                verify_on_restore: verify,
            })?;
        }
    }
    Ok(())
}

fn cmd_list_disks() -> anyhow::Result<()> {
    let mut disks = enumerate_disks()?;
    for disk in &mut disks {
        for part in &mut disk.partitions {
            phoenix_core::disk::refine_partition_fs(part);
        }
        println!("Disk {}: {} ({:.2} GB, {})", 
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
