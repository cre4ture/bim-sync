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

#[derive(Debug, Clone, Copy)]
struct SyncOptions {
    block_size: u64,
    verify_only: bool,
    verify_writes: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SyncSummary {
    checked_bytes: u64,
    changed_bytes: u64,
    different_blocks: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum SyncEvent {
    Diff {
        offset: u64,
        length: usize,
    },
    Wrote {
        offset: u64,
        length: usize,
        verified: bool,
    },
    Progress {
        checked_bytes: u64,
        changed_bytes: u64,
        image_size: u64,
    },
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
    let block_size = args
        .block_size_mib
        .checked_mul(1024 * 1024)
        .context("Block size is too large")?;

    println!("Image:       {:?}", args.image);
    println!("Target disk: {}", disk_path);
    println!("Image size:  {} bytes", image_size);
    println!("Block size:  {} bytes", block_size);
    println!("Verify only: {}", args.verify_only);
    println!();

    println!(
        "WARNING: Make absolutely sure PhysicalDrive{} is your SD card.",
        args.disk
    );
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
                    "Could not open {disk_path} for read/write. Run as Administrator and make sure the disk is offline."
                )
            })?
    };

    let summary = sync_image_to_disk(
        &mut image,
        &mut disk,
        image_size,
        SyncOptions {
            block_size,
            verify_only: args.verify_only,
            verify_writes: !args.no_verify_writes,
        },
        |event| match event {
            SyncEvent::Diff { offset, length } => {
                println!("DIFF offset={} length={}", offset, length);
            }
            SyncEvent::Wrote {
                offset,
                length,
                verified,
            } => {
                if verified {
                    println!("WROTE+VERIFIED offset={} length={}", offset, length);
                } else {
                    println!("WROTE offset={} length={}", offset, length);
                }
            }
            SyncEvent::Progress {
                checked_bytes,
                changed_bytes,
                image_size,
            } => {
                let pct = checked_bytes as f64 * 100.0 / image_size as f64;
                println!(
                    "Progress: {:.2}% checked, {:.1} MiB different",
                    pct,
                    changed_bytes as f64 / 1024.0 / 1024.0
                );
            }
        },
    )?;

    println!();
    println!("Done.");
    println!("Blocks different: {}", summary.different_blocks);
    println!(
        "Bytes different:  {} bytes / {:.1} MiB",
        summary.changed_bytes,
        summary.changed_bytes as f64 / 1024.0 / 1024.0
    );

    Ok(())
}

fn sync_image_to_disk<I, D, F>(
    image: &mut I,
    disk: &mut D,
    image_size: u64,
    options: SyncOptions,
    mut report: F,
) -> Result<SyncSummary>
where
    I: Read,
    D: Read + Write + Seek,
    F: FnMut(SyncEvent),
{
    if options.block_size == 0 {
        bail!("Block size must be greater than zero");
    }

    let block_size =
        usize::try_from(options.block_size).context("Block size is too large for this platform")?;
    let mut img_buf = vec![0u8; block_size];
    let mut disk_buf = vec![0u8; block_size];

    let mut offset: u64 = 0;
    let mut summary = SyncSummary::default();

    while offset < image_size {
        let remaining = image_size - offset;
        let to_read = remaining.min(options.block_size) as usize;

        let img_read = read_exact_or_eof(image, &mut img_buf[..to_read])
            .with_context(|| format!("Could not read image at offset {}", offset))?;

        if img_read == 0 {
            break;
        }

        disk.seek(SeekFrom::Start(offset))
            .with_context(|| format!("Could not seek target disk to offset {}", offset))?;

        read_exact_full(disk, &mut disk_buf[..img_read])
            .with_context(|| format!("Could not read target disk at offset {}", offset))?;

        if img_buf[..img_read] != disk_buf[..img_read] {
            summary.different_blocks += 1;
            summary.changed_bytes += img_read as u64;

            if options.verify_only {
                report(SyncEvent::Diff {
                    offset,
                    length: img_read,
                });
            } else {
                disk.seek(SeekFrom::Start(offset)).with_context(|| {
                    format!("Could not seek target disk to write offset {}", offset)
                })?;

                disk.write_all(&img_buf[..img_read])
                    .with_context(|| format!("Could not write target disk at offset {}", offset))?;

                disk.flush()
                    .with_context(|| format!("Could not flush target disk at offset {}", offset))?;

                if options.verify_writes {
                    disk.seek(SeekFrom::Start(offset)).with_context(|| {
                        format!("Could not seek target disk to verify offset {}", offset)
                    })?;

                    let mut verify_buf = vec![0u8; img_read];

                    read_exact_full(disk, &mut verify_buf).with_context(|| {
                        format!("Could not verify-read target disk at offset {}", offset)
                    })?;

                    if img_buf[..img_read] != verify_buf[..] {
                        bail!("Verify mismatch at offset {}", offset);
                    }

                    report(SyncEvent::Wrote {
                        offset,
                        length: img_read,
                        verified: true,
                    });
                } else {
                    report(SyncEvent::Wrote {
                        offset,
                        length: img_read,
                        verified: false,
                    });
                }
            }
        }

        offset += img_read as u64;
        summary.checked_bytes += img_read as u64;

        if summary.checked_bytes % (512 * 1024 * 1024) < options.block_size {
            report(SyncEvent::Progress {
                checked_bytes: summary.checked_bytes,
                changed_bytes: summary.changed_bytes,
                image_size,
            });
        }
    }

    Ok(summary)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Error, ErrorKind};

    fn write_events(events: &[SyncEvent]) -> Vec<&SyncEvent> {
        events
            .iter()
            .filter(|event| matches!(event, SyncEvent::Diff { .. } | SyncEvent::Wrote { .. }))
            .collect()
    }

    #[test]
    fn cli_parses_required_args_and_defaults() {
        let args = Args::try_parse_from([
            "raw-image-sync",
            "--image",
            r"C:\images\sdcard.img",
            "--disk",
            "7",
        ])
        .unwrap();

        assert_eq!(args.image, PathBuf::from(r"C:\images\sdcard.img"));
        assert_eq!(args.disk, 7);
        assert_eq!(args.block_size_mib, 4);
        assert!(!args.verify_only);
        assert!(!args.no_verify_writes);
    }

    #[test]
    fn cli_parses_optional_flags() {
        let args = Args::try_parse_from([
            "raw-image-sync",
            "--image",
            r"C:\images\sdcard.img",
            "--disk",
            "1",
            "--block-size-mib",
            "16",
            "--verify-only",
            "--no-verify-writes",
        ])
        .unwrap();

        assert_eq!(args.block_size_mib, 16);
        assert!(args.verify_only);
        assert!(args.no_verify_writes);
    }

    #[test]
    fn verify_only_reports_differences_without_writing() {
        let image_bytes = vec![1, 2, 3, 4, 5];
        let disk_bytes = vec![1, 2, 0, 4, 0];
        let mut image = Cursor::new(image_bytes);
        let mut disk = Cursor::new(disk_bytes.clone());
        let mut events = Vec::new();

        let summary = sync_image_to_disk(
            &mut image,
            &mut disk,
            5,
            SyncOptions {
                block_size: 2,
                verify_only: true,
                verify_writes: true,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(
            summary,
            SyncSummary {
                checked_bytes: 5,
                changed_bytes: 3,
                different_blocks: 2,
            }
        );
        assert_eq!(disk.into_inner(), disk_bytes);
        assert_eq!(
            write_events(&events),
            vec![
                &SyncEvent::Diff {
                    offset: 2,
                    length: 2,
                },
                &SyncEvent::Diff {
                    offset: 4,
                    length: 1,
                },
            ]
        );
    }

    #[test]
    fn sync_writes_only_changed_blocks_and_verifies() {
        let image_bytes = vec![1, 2, 3, 4, 5, 6];
        let mut image = Cursor::new(image_bytes.clone());
        let mut disk = Cursor::new(vec![1, 2, 0, 4, 0, 0]);
        let mut events = Vec::new();

        let summary = sync_image_to_disk(
            &mut image,
            &mut disk,
            image_bytes.len() as u64,
            SyncOptions {
                block_size: 2,
                verify_only: false,
                verify_writes: true,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(disk.into_inner(), image_bytes);
        assert_eq!(
            summary,
            SyncSummary {
                checked_bytes: 6,
                changed_bytes: 4,
                different_blocks: 2,
            }
        );
        assert_eq!(
            write_events(&events),
            vec![
                &SyncEvent::Wrote {
                    offset: 2,
                    length: 2,
                    verified: true,
                },
                &SyncEvent::Wrote {
                    offset: 4,
                    length: 2,
                    verified: true,
                },
            ]
        );
    }

    #[test]
    fn sync_can_skip_write_verification() {
        let image_bytes = vec![9, 8, 7];
        let mut image = Cursor::new(image_bytes.clone());
        let mut disk = Cursor::new(vec![0, 8, 0]);
        let mut events = Vec::new();

        let summary = sync_image_to_disk(
            &mut image,
            &mut disk,
            image_bytes.len() as u64,
            SyncOptions {
                block_size: 1,
                verify_only: false,
                verify_writes: false,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(disk.into_inner(), image_bytes);
        assert_eq!(
            summary,
            SyncSummary {
                checked_bytes: 3,
                changed_bytes: 2,
                different_blocks: 2,
            }
        );
        assert_eq!(
            write_events(&events),
            vec![
                &SyncEvent::Wrote {
                    offset: 0,
                    length: 1,
                    verified: false,
                },
                &SyncEvent::Wrote {
                    offset: 2,
                    length: 1,
                    verified: false,
                },
            ]
        );
    }

    #[test]
    fn identical_target_skips_all_writes() {
        let image_bytes = vec![1, 1, 2, 3, 5, 8, 13];
        let mut image = Cursor::new(image_bytes.clone());
        let mut disk = Cursor::new(image_bytes.clone());
        let mut events = Vec::new();

        let summary = sync_image_to_disk(
            &mut image,
            &mut disk,
            image_bytes.len() as u64,
            SyncOptions {
                block_size: 3,
                verify_only: false,
                verify_writes: true,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(disk.into_inner(), image_bytes);
        assert_eq!(
            summary,
            SyncSummary {
                checked_bytes: 7,
                changed_bytes: 0,
                different_blocks: 0,
            }
        );
        assert!(write_events(&events).is_empty());
    }

    #[test]
    fn verification_failure_is_reported() {
        let mut image = Cursor::new(vec![1, 2, 3]);
        let mut disk = CorruptingDisk::new(vec![0, 0, 0]);
        let mut events = Vec::new();

        let err = sync_image_to_disk(
            &mut image,
            &mut disk,
            3,
            SyncOptions {
                block_size: 3,
                verify_only: false,
                verify_writes: true,
            },
            |event| events.push(event),
        )
        .unwrap_err();

        assert!(err.to_string().contains("Verify mismatch at offset 0"));
        assert!(write_events(&events).is_empty());
    }

    #[test]
    fn short_target_read_is_reported() {
        let mut image = Cursor::new(vec![1, 2, 3, 4]);
        let mut disk = Cursor::new(vec![1, 2]);
        let mut events = Vec::new();

        let err = sync_image_to_disk(
            &mut image,
            &mut disk,
            4,
            SyncOptions {
                block_size: 4,
                verify_only: true,
                verify_writes: true,
            },
            |event| events.push(event),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("Could not read target disk at offset 0"));
        assert!(events.is_empty());
    }

    #[test]
    fn zero_block_size_is_rejected() {
        let mut image = Cursor::new(vec![1, 2, 3]);
        let mut disk = Cursor::new(vec![1, 2, 3]);
        let mut events = Vec::new();

        let err = sync_image_to_disk(
            &mut image,
            &mut disk,
            3,
            SyncOptions {
                block_size: 0,
                verify_only: false,
                verify_writes: true,
            },
            |event| events.push(event),
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "Block size must be greater than zero");
        assert!(events.is_empty());
    }

    #[test]
    fn read_exact_or_eof_returns_partial_final_read() {
        let mut reader = Cursor::new(vec![1, 2, 3]);
        let mut buf = [0; 5];

        let read = read_exact_or_eof(&mut reader, &mut buf).unwrap();

        assert_eq!(read, 3);
        assert_eq!(&buf[..3], &[1, 2, 3]);
        assert_eq!(&buf[3..], &[0, 0]);
    }

    struct CorruptingDisk {
        inner: Cursor<Vec<u8>>,
    }

    impl CorruptingDisk {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
            }
        }
    }

    impl Read for CorruptingDisk {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl Write for CorruptingDisk {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }

            let mut corrupted = buf.to_vec();
            corrupted[0] = corrupted[0].wrapping_add(1);
            self.inner.write(&corrupted)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for CorruptingDisk {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn read_exact_full_rejects_short_reads() {
        let mut reader = ShortRead::new(vec![1, 2]);
        let mut buf = [0; 3];

        let err = read_exact_full(&mut reader, &mut buf).unwrap_err();

        assert_eq!(
            err.downcast_ref::<Error>().unwrap().kind(),
            ErrorKind::UnexpectedEof
        );
    }

    struct ShortRead {
        inner: Cursor<Vec<u8>>,
    }

    impl ShortRead {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
            }
        }
    }

    impl Read for ShortRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }
}
