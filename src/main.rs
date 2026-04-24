use anyhow::{bail, Context, Result};
use clap::Parser;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "raw-image-sync",
    about = "Incrementally sync a raw image file to a Windows physical disk"
)]
struct Args {
    /// Path to the raw image file, for example C:\images\sdcard.img
    #[arg(short, long)]
    image: PathBuf,

    /// Windows physical disk number, for example 1 for \\.\PhysicalDrive1
    #[arg(short, long)]
    disk: u32,

    /// Block size in MiB
    #[arg(short = 'b', long, default_value_t = 4)]
    block_size_mib: u64,

    /// Compare only; do not write
    #[arg(long)]
    verify_only: bool,

    /// Skip read-after-write verification
    #[arg(long)]
    no_verify_writes: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.block_size_mib == 0 {
        bail!("Block size must be greater than zero");
    }

    let image_size = std::fs::metadata(&args.image)
        .with_context(|| format!("Could not stat image file {:?}", args.image))?
        .len();

    let disk_path = format!(r"\\.\PhysicalDrive{}", args.disk);
    let block_size = args.block_size_mib * 1024 * 1024;

    println!("Image:       {:?}", args.image);
    println!("Target disk: {}", disk_path);
    println!("Image size:  {} bytes", image_size);
    println!("Block size:  {} bytes", block_size);
    println!("Verify only: {}", args.verify_only);
    println!();

    println!("WARNING: Make absolutely sure PhysicalDrive{} is your SD card.", args.disk);
    println!("This program can overwrite disks.");
    println!();

    let mut image = File::open(&args.image)
        .with_context(|| format!("Could not open image file {:?}", args.image))?;

    let mut disk = if args.verify_only {
        OpenOptions::new()
            .read(true)
            .open(&disk_path)
            .with_context(|| format!("Could not open {} for reading", disk_path))?
    } else {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk_path)
            .with_context(|| {
                format!(
                    "Could not open {} for read/write. Run as Administrator and make sure the disk is offline.",
                    disk_path
                )
            })?
    };

    let mut img_buf = vec![0u8; block_size as usize];
    let mut disk_buf = vec![0u8; block_size as usize];

    let mut offset: u64 = 0;
    let mut checked_bytes: u64 = 0;
    let mut changed_bytes: u64 = 0;
    let mut different_blocks: u64 = 0;

    while offset < image_size {
        let remaining = image_size - offset;
        let to_read = remaining.min(block_size) as usize;

        let img_read = read_exact_or_eof(&mut image, &mut img_buf[..to_read])
            .with_context(|| format!("Could not read image at offset {}", offset))?;

        if img_read == 0 {
            break;
        }

        disk.seek(SeekFrom::Start(offset))
            .with_context(|| format!("Could not seek target disk to offset {}", offset))?;

        read_exact_full(&mut disk, &mut disk_buf[..img_read])
            .with_context(|| format!("Could not read target disk at offset {}", offset))?;

        if img_buf[..img_read] != disk_buf[..img_read] {
            different_blocks += 1;
            changed_bytes += img_read as u64;

            if args.verify_only {
                println!("DIFF offset={} length={}", offset, img_read);
            } else {
                disk.seek(SeekFrom::Start(offset))
                    .with_context(|| format!("Could not seek target disk to write offset {}", offset))?;

                disk.write_all(&img_buf[..img_read])
                    .with_context(|| format!("Could not write target disk at offset {}", offset))?;

                disk.flush()
                    .with_context(|| format!("Could not flush target disk at offset {}", offset))?;

                if !args.no_verify_writes {
                    disk.seek(SeekFrom::Start(offset))
                        .with_context(|| format!("Could not seek target disk to verify offset {}", offset))?;

                    let mut verify_buf = vec![0u8; img_read];

                    read_exact_full(&mut disk, &mut verify_buf)
                        .with_context(|| format!("Could not verify-read target disk at offset {}", offset))?;

                    if img_buf[..img_read] != verify_buf[..] {
                        bail!("Verify mismatch at offset {}", offset);
                    }

                    println!("WROTE+VERIFIED offset={} length={}", offset, img_read);
                } else {
                    println!("WROTE offset={} length={}", offset, img_read);
                }
            }
        }

        offset += img_read as u64;
        checked_bytes += img_read as u64;

        if checked_bytes % (512 * 1024 * 1024) < block_size {
            let pct = checked_bytes as f64 * 100.0 / image_size as f64;
            println!(
                "Progress: {:.2}% checked, {:.1} MiB different",
                pct,
                changed_bytes as f64 / 1024.0 / 1024.0
            );
        }
    }

    println!();
    println!("Done.");
    println!("Blocks different: {}", different_blocks);
    println!(
        "Bytes different:  {} bytes / {:.1} MiB",
        changed_bytes,
        changed_bytes as f64 / 1024.0 / 1024.0
    );

    Ok(())
}

/// Reads until the buffer is full or EOF is reached.
/// Returns how many bytes were read.
fn read_exact_or_eof<R: Read>(reader: &mut R, mut buf: &mut [u8]) -> Result<usize> {
    let original_len = buf.len();
    let mut total = 0;

    while !buf.is_empty() {
        match reader.read(buf)? {
            0 => break,
            n => {
                total += n;
                let tmp = buf;
                buf = &mut tmp[n..];
            }
        }
    }

    if total == 0 {
        Ok(0)
    } else {
        Ok(original_len - buf.len())
    }
}

/// Reads exactly the full buffer or returns an error.
fn read_exact_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<()> {
    reader.read_exact(buf)?;
    Ok(())
}