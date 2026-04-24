use anyhow::{bail, Context, Result};
use clap::Parser;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "bim-sync",
    about = "Incrementally sync a raw image file to a Windows physical disk"
)]
struct Args {
    /// Path to the raw image file, for example C:\images\sdcard.img
    #[arg(
        short,
        long,
        required_unless_present = "manual_test",
        conflicts_with = "manual_test"
    )]
    image: Option<PathBuf>,

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

    /// Run a destructive generated-image test against the target disk
    #[arg(long, conflicts_with_all = ["verify_only", "no_verify_writes"])]
    manual_test: bool,
}

const MANUAL_TEST_BLOCK_COUNT: u64 = 2;
const MANUAL_TEST_MUTATION_LEN: usize = 32;

#[derive(Debug, Clone, Copy)]
struct SyncOptions {
    block_size: u64,
    verify_only: bool,
    verify_writes: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SyncSummary {
    checked_bytes: u64,
    differing_bytes: u64,
    rewrite_bytes: u64,
    different_blocks: u64,
}

impl SyncSummary {
    fn skipped_bytes(self) -> u64 {
        self.checked_bytes.saturating_sub(self.rewrite_bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        differing_bytes: u64,
        rewrite_bytes: u64,
        image_size: u64,
    },
}

#[derive(Debug, Clone, Copy)]
struct ManualTestOptions {
    test_image_size: usize,
    block_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManualTestSummary {
    image_size: u64,
    mutation_offset: u64,
    mutation_length: usize,
    initial_write: SyncSummary,
    initial_verify: SyncSummary,
    modified_verify: SyncSummary,
    repair: SyncSummary,
    repaired_verify: SyncSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualTestPhase {
    InitialWrite,
    VerifyInitialWrite,
    ModifyDisk,
    VerifyModifiedDisk,
    RepairDisk,
    VerifyRepairedDisk,
}

impl ManualTestPhase {
    fn label(self) -> &'static str {
        match self {
            ManualTestPhase::InitialWrite => "initial write",
            ManualTestPhase::VerifyInitialWrite => "verify initial write",
            ManualTestPhase::ModifyDisk => "modify disk",
            ManualTestPhase::VerifyModifiedDisk => "verify modified disk",
            ManualTestPhase::RepairDisk => "repair disk",
            ManualTestPhase::VerifyRepairedDisk => "verify repaired disk",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualTestEvent {
    PhaseStarted(ManualTestPhase),
    Sync {
        phase: ManualTestPhase,
        event: SyncEvent,
    },
    PhaseSummary {
        phase: ManualTestPhase,
        summary: SyncSummary,
    },
    Modified {
        offset: u64,
        length: usize,
    },
    PhaseCompleted(ManualTestPhase),
}

fn main() -> Result<()> {
    let args = Args::parse();

    let disk_path = format!(r"\\.\PhysicalDrive{}", args.disk);
    let block_size = block_size_bytes(args.block_size_mib)?;

    if args.manual_test {
        run_manual_test_mode(args.disk, &disk_path, block_size)
    } else {
        run_sync_mode(&args, &disk_path, block_size)
    }
}

fn block_size_bytes(block_size_mib: u64) -> Result<u64> {
    if block_size_mib == 0 {
        bail!("Block size must be greater than zero");
    }

    block_size_mib
        .checked_mul(1024 * 1024)
        .context("Block size is too large")
}

fn run_sync_mode(args: &Args, disk_path: &str, block_size: u64) -> Result<()> {
    let image_path = args
        .image
        .as_ref()
        .context("--image is required unless --manual-test is used")?;
    let image_size = std::fs::metadata(image_path)
        .with_context(|| format!("Could not stat image file {:?}", image_path))?
        .len();

    println!("Image:       {:?}", image_path);
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

    let mut image = File::open(image_path)
        .with_context(|| format!("Could not open image file {:?}", image_path))?;

    let mut disk = if args.verify_only {
        OpenOptions::new()
            .read(true)
            .open(disk_path)
            .with_context(|| format!("Could not open {} for reading", disk_path))?
    } else {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_path)
            .with_context(|| {
                format!(
                    "Could not open {disk_path} for read/write. Run as Administrator and make sure the disk is offline, or for removable media, that its volumes are dismounted."
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
                println!("DIFF block_offset={} block_length={}", offset, length);
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
                differing_bytes,
                rewrite_bytes,
                image_size,
            } => {
                let pct = checked_bytes as f64 * 100.0 / image_size as f64;
                println!(
                    "Progress: {:.2}% checked, {:.1} MiB byte differences, {:.1} MiB in differing blocks",
                    pct,
                    differing_bytes as f64 / 1024.0 / 1024.0,
                    rewrite_bytes as f64 / 1024.0 / 1024.0
                );
            }
        },
    )?;

    println!();
    println!("Done.");
    println!("Blocks different: {}", summary.different_blocks);
    println!(
        "Byte differences: {} bytes / {:.1} MiB",
        summary.differing_bytes,
        summary.differing_bytes as f64 / 1024.0 / 1024.0
    );
    println!(
        "Bytes in differing blocks: {} bytes / {:.1} MiB",
        summary.rewrite_bytes,
        summary.rewrite_bytes as f64 / 1024.0 / 1024.0
    );
    println!(
        "Bytes skipped: {} bytes / {:.1} MiB",
        summary.skipped_bytes(),
        summary.skipped_bytes() as f64 / 1024.0 / 1024.0
    );

    Ok(())
}

fn run_manual_test_mode(disk_number: u32, disk_path: &str, block_size: u64) -> Result<()> {
    let test_image_size = manual_test_image_size(block_size)?;

    println!("Manual SD-card test mode");
    println!("Target disk: {}", disk_path);
    println!(
        "Test image size: {} bytes ({} sync blocks)",
        test_image_size, MANUAL_TEST_BLOCK_COUNT
    );
    println!("Block size: {} bytes", block_size);
    println!();
    println!(
        "WARNING: This overwrites the first {} bytes of PhysicalDrive{}.",
        test_image_size, disk_number
    );
    println!("Use only a disposable SD card selected on purpose.");
    println!();

    let mut disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk_path)
        .with_context(|| {
            format!(
                "Could not open {disk_path} for read/write. Run as Administrator and make sure the disk is offline, or for removable media, that its volumes are dismounted."
            )
        })?;

    let summary = run_manual_sd_test(
        &mut disk,
        ManualTestOptions {
            test_image_size,
            block_size,
        },
        print_manual_test_event,
    )?;

    println!();
    println!("Manual test complete.");
    println!(
        "Modified {} bytes at offset {}, then repaired the target.",
        summary.mutation_length, summary.mutation_offset
    );

    Ok(())
}

fn print_manual_test_event(event: ManualTestEvent) {
    match event {
        ManualTestEvent::PhaseStarted(phase) => {
            println!("== {} ==", phase.label());
        }
        ManualTestEvent::Sync { phase, event } => match event {
            SyncEvent::Diff { offset, length } => {
                println!(
                    "{}: DIFF block_offset={} block_length={}",
                    phase.label(),
                    offset,
                    length
                );
            }
            SyncEvent::Wrote {
                offset,
                length,
                verified,
            } => {
                if verified {
                    println!(
                        "{}: WROTE+VERIFIED offset={} length={}",
                        phase.label(),
                        offset,
                        length
                    );
                } else {
                    println!(
                        "{}: WROTE offset={} length={}",
                        phase.label(),
                        offset,
                        length
                    );
                }
            }
            SyncEvent::Progress { .. } => {}
        },
        ManualTestEvent::PhaseSummary { phase, summary } => {
            println!(
                "{} summary: {} bytes checked, {} differing blocks, {} byte differences, {} bytes in differing blocks, {} bytes skipped",
                phase.label(),
                summary.checked_bytes,
                summary.different_blocks,
                summary.differing_bytes,
                summary.rewrite_bytes,
                summary.skipped_bytes()
            );
        }
        ManualTestEvent::Modified { offset, length } => {
            println!(
                "modified target bytes at offset={} length={}",
                offset, length
            );
        }
        ManualTestEvent::PhaseCompleted(phase) => {
            println!("{} complete", phase.label());
        }
    }
}

fn run_manual_sd_test<D, F>(
    disk: &mut D,
    options: ManualTestOptions,
    mut report: F,
) -> Result<ManualTestSummary>
where
    D: Read + Write + Seek,
    F: FnMut(ManualTestEvent),
{
    let image = manual_test_image(options.test_image_size)?;
    let mutation_offset = manual_test_mutation_offset(image.len(), options.block_size)?;
    let mutation = manual_test_mutation(&image, mutation_offset, MANUAL_TEST_MUTATION_LEN)?;

    let initial_write = run_manual_sync_phase(
        disk,
        &image,
        options.block_size,
        false,
        ManualTestPhase::InitialWrite,
        &mut report,
    )?;

    let initial_verify = run_manual_sync_phase(
        disk,
        &image,
        options.block_size,
        true,
        ManualTestPhase::VerifyInitialWrite,
        &mut report,
    )?;
    ensure_no_differences(ManualTestPhase::VerifyInitialWrite, initial_verify)?;

    report(ManualTestEvent::PhaseStarted(ManualTestPhase::ModifyDisk));
    write_manual_test_mutation(disk, &image, options.block_size, mutation_offset, &mutation)
        .context("Could not modify target disk")?;
    report(ManualTestEvent::Modified {
        offset: mutation_offset,
        length: mutation.len(),
    });
    report(ManualTestEvent::PhaseCompleted(ManualTestPhase::ModifyDisk));

    let modified_verify = run_manual_sync_phase(
        disk,
        &image,
        options.block_size,
        true,
        ManualTestPhase::VerifyModifiedDisk,
        &mut report,
    )?;
    if modified_verify.different_blocks == 0 {
        bail!("Manual test modification was not detected");
    }

    let repair = run_manual_sync_phase(
        disk,
        &image,
        options.block_size,
        false,
        ManualTestPhase::RepairDisk,
        &mut report,
    )?;

    let repaired_verify = run_manual_sync_phase(
        disk,
        &image,
        options.block_size,
        true,
        ManualTestPhase::VerifyRepairedDisk,
        &mut report,
    )?;
    ensure_no_differences(ManualTestPhase::VerifyRepairedDisk, repaired_verify)?;

    Ok(ManualTestSummary {
        image_size: image.len() as u64,
        mutation_offset,
        mutation_length: mutation.len(),
        initial_write,
        initial_verify,
        modified_verify,
        repair,
        repaired_verify,
    })
}

fn run_manual_sync_phase<D, F>(
    disk: &mut D,
    image: &[u8],
    block_size: u64,
    verify_only: bool,
    phase: ManualTestPhase,
    report: &mut F,
) -> Result<SyncSummary>
where
    D: Read + Write + Seek,
    F: FnMut(ManualTestEvent),
{
    report(ManualTestEvent::PhaseStarted(phase));

    let mut image_reader = std::io::Cursor::new(image.to_vec());
    let summary = sync_image_to_disk(
        &mut image_reader,
        disk,
        image.len() as u64,
        SyncOptions {
            block_size,
            verify_only,
            verify_writes: true,
        },
        |event| report(ManualTestEvent::Sync { phase, event }),
    )?;

    report(ManualTestEvent::PhaseSummary { phase, summary });

    Ok(summary)
}

fn ensure_no_differences(phase: ManualTestPhase, summary: SyncSummary) -> Result<()> {
    if summary.different_blocks != 0 {
        bail!(
            "Manual test phase '{}' found {} differing blocks",
            phase.label(),
            summary.different_blocks
        );
    }

    Ok(())
}

fn manual_test_image_size(block_size: u64) -> Result<usize> {
    let image_size = block_size
        .checked_mul(MANUAL_TEST_BLOCK_COUNT)
        .context("Manual test image size is too large")?;

    usize::try_from(image_size).context("Manual test image size is too large for this platform")
}

fn manual_test_image(size: usize) -> Result<Vec<u8>> {
    if size < MANUAL_TEST_MUTATION_LEN {
        bail!(
            "Manual test image must be at least {} bytes",
            MANUAL_TEST_MUTATION_LEN
        );
    }

    Ok((0..size)
        .map(|index| {
            let value = index as u32;
            (value
                .wrapping_mul(37)
                .wrapping_add(value.rotate_left(5))
                .wrapping_add(0xA5)) as u8
        })
        .collect())
}

fn manual_test_mutation_offset(image_size: usize, block_size: u64) -> Result<u64> {
    if image_size < MANUAL_TEST_MUTATION_LEN {
        bail!(
            "Manual test image must be at least {} bytes",
            MANUAL_TEST_MUTATION_LEN
        );
    }

    let block_size = usize::try_from(block_size)
        .context("Manual test block size is too large for this platform")?;

    if block_size < MANUAL_TEST_MUTATION_LEN {
        bail!(
            "Manual test block size must be at least {} bytes",
            MANUAL_TEST_MUTATION_LEN
        );
    }

    if image_size < block_size {
        bail!("Manual test image must be at least one block");
    }

    Ok(((block_size - MANUAL_TEST_MUTATION_LEN) / 2) as u64)
}

fn manual_test_mutation(image: &[u8], offset: u64, length: usize) -> Result<Vec<u8>> {
    let start = usize::try_from(offset).context("Manual test mutation offset is too large")?;
    let end = start
        .checked_add(length)
        .context("Manual test mutation range is too large")?;

    if end > image.len() {
        bail!("Manual test mutation range exceeds the test image");
    }

    Ok(image[start..end].iter().map(|byte| byte ^ 0xA5).collect())
}

fn write_manual_test_mutation<W>(
    writer: &mut W,
    image: &[u8],
    block_size: u64,
    offset: u64,
    mutation: &[u8],
) -> Result<()>
where
    W: Write + Seek,
{
    let block_size = usize::try_from(block_size)
        .context("Manual test block size is too large for this platform")?;
    let start = usize::try_from(offset).context("Manual test mutation offset is too large")?;
    let end = start
        .checked_add(mutation.len())
        .context("Manual test mutation range is too large")?;

    if end > image.len() {
        bail!("Manual test mutation range exceeds the test image");
    }

    let block_start = start / block_size * block_size;
    let block_end = block_start
        .checked_add(block_size)
        .context("Manual test mutation block range is too large")?;

    if block_end > image.len() {
        bail!("Manual test mutation block range exceeds the test image");
    }

    if end > block_end {
        bail!("Manual test mutation crosses a sync block boundary");
    }

    let mut mutated_block = image[block_start..block_end].to_vec();
    mutated_block[start - block_start..end - block_start].copy_from_slice(mutation);

    write_exact_at(writer, block_start as u64, &mutated_block)
        .context("Could not write mutated manual-test block")
}

fn write_exact_at<W>(writer: &mut W, offset: u64, bytes: &[u8]) -> Result<()>
where
    W: Write + Seek,
{
    writer
        .seek(SeekFrom::Start(offset))
        .with_context(|| format!("Could not seek target disk to offset {}", offset))?;
    writer
        .write_all(bytes)
        .with_context(|| format!("Could not write target disk at offset {}", offset))?;
    writer
        .flush()
        .with_context(|| format!("Could not flush target disk at offset {}", offset))?;

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
            let block_differing_bytes =
                count_differing_bytes(&img_buf[..img_read], &disk_buf[..img_read]) as u64;
            summary.different_blocks += 1;
            summary.differing_bytes += block_differing_bytes;
            summary.rewrite_bytes += img_read as u64;

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
                differing_bytes: summary.differing_bytes,
                rewrite_bytes: summary.rewrite_bytes,
                image_size,
            });
        }
    }

    Ok(summary)
}

fn count_differing_bytes(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .filter(|(left, right)| *left != *right)
        .count()
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
            "bim-sync",
            "--image",
            r"C:\images\sdcard.img",
            "--disk",
            "7",
        ])
        .unwrap();

        assert_eq!(args.image, Some(PathBuf::from(r"C:\images\sdcard.img")));
        assert_eq!(args.disk, 7);
        assert_eq!(args.block_size_mib, 4);
        assert!(!args.verify_only);
        assert!(!args.no_verify_writes);
        assert!(!args.manual_test);
    }

    #[test]
    fn cli_parses_optional_flags() {
        let args = Args::try_parse_from([
            "bim-sync",
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
        assert!(!args.manual_test);
    }

    #[test]
    fn cli_parses_manual_test_without_image() {
        let args = Args::try_parse_from(["bim-sync", "--disk", "1", "--manual-test"]).unwrap();

        assert_eq!(args.image, None);
        assert_eq!(args.disk, 1);
        assert!(args.manual_test);
    }

    #[test]
    fn cli_requires_image_for_normal_sync() {
        let err = Args::try_parse_from(["bim-sync", "--disk", "1"]).unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn cli_rejects_manual_test_with_normal_sync_args() {
        let err = Args::try_parse_from([
            "bim-sync",
            "--disk",
            "1",
            "--manual-test",
            "--image",
            r"C:\images\sdcard.img",
        ])
        .unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn manual_test_workflow_writes_detects_and_repairs_target() {
        let image_size = manual_test_image_size(64).unwrap();
        let expected_image = manual_test_image(image_size).unwrap();
        let mut disk = Cursor::new(vec![0; image_size]);
        let mut events = Vec::new();

        let summary = run_manual_sd_test(
            &mut disk,
            ManualTestOptions {
                test_image_size: image_size,
                block_size: 64,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(disk.into_inner(), expected_image);
        assert_eq!(summary.image_size, image_size as u64);
        assert_eq!(summary.mutation_length, MANUAL_TEST_MUTATION_LEN);
        assert_eq!(summary.initial_verify.different_blocks, 0);
        assert_eq!(
            summary.modified_verify.differing_bytes,
            MANUAL_TEST_MUTATION_LEN as u64
        );
        assert_eq!(summary.modified_verify.rewrite_bytes, 64);
        assert_eq!(summary.modified_verify.skipped_bytes(), 64);
        assert!(summary.modified_verify.different_blocks > 0);
        assert!(summary.repair.different_blocks > 0);
        assert_eq!(summary.repair.rewrite_bytes, 64);
        assert_eq!(summary.repair.skipped_bytes(), 64);
        assert_eq!(summary.repaired_verify.different_blocks, 0);
        assert!(events
            .iter()
            .any(|event| matches!(event, ManualTestEvent::Modified { .. })));
    }

    #[test]
    fn manual_test_mutation_uses_raw_disk_aligned_write() {
        let image_size = manual_test_image_size(64).unwrap();
        let expected_image = manual_test_image(image_size).unwrap();
        let mut disk = AlignedWriteDisk::new(vec![0; image_size], 64);
        let mut events = Vec::new();

        let summary = run_manual_sd_test(
            &mut disk,
            ManualTestOptions {
                test_image_size: image_size,
                block_size: 64,
            },
            |event| events.push(event),
        )
        .unwrap();

        assert_eq!(disk.into_inner(), expected_image);
        assert_eq!(summary.mutation_offset, 16);
        assert_eq!(summary.mutation_length, MANUAL_TEST_MUTATION_LEN);
        assert_eq!(
            summary.modified_verify.differing_bytes,
            MANUAL_TEST_MUTATION_LEN as u64
        );
        assert_eq!(summary.modified_verify.rewrite_bytes, 64);
        assert_eq!(summary.modified_verify.skipped_bytes(), 64);
        assert!(events
            .iter()
            .any(|event| matches!(event, ManualTestEvent::Modified { .. })));
    }

    #[test]
    fn manual_test_rejects_too_small_test_image() {
        let mut disk = Cursor::new(vec![0; MANUAL_TEST_MUTATION_LEN - 1]);
        let mut events = Vec::new();

        let err = run_manual_sd_test(
            &mut disk,
            ManualTestOptions {
                test_image_size: MANUAL_TEST_MUTATION_LEN - 1,
                block_size: 64,
            },
            |event| events.push(event),
        )
        .unwrap_err();

        assert!(err.to_string().contains("Manual test image must be"));
        assert!(events.is_empty());
    }

    #[test]
    fn manual_test_reports_short_target_read() {
        let image_size = manual_test_image_size(64).unwrap();
        let mut disk = Cursor::new(vec![0; image_size - 1]);
        let mut events = Vec::new();

        let err = run_manual_sd_test(
            &mut disk,
            ManualTestOptions {
                test_image_size: image_size,
                block_size: 64,
            },
            |event| events.push(event),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("Could not read target disk at offset"));
        assert_eq!(
            events.first(),
            Some(&ManualTestEvent::PhaseStarted(
                ManualTestPhase::InitialWrite
            ))
        );
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
                differing_bytes: 2,
                rewrite_bytes: 3,
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
                differing_bytes: 3,
                rewrite_bytes: 4,
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
                differing_bytes: 2,
                rewrite_bytes: 2,
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
                differing_bytes: 0,
                rewrite_bytes: 0,
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

    struct AlignedWriteDisk {
        inner: Cursor<Vec<u8>>,
        alignment: u64,
    }

    impl AlignedWriteDisk {
        fn new(bytes: Vec<u8>, alignment: u64) -> Self {
            Self {
                inner: Cursor::new(bytes),
                alignment,
            }
        }

        fn into_inner(self) -> Vec<u8> {
            self.inner.into_inner()
        }
    }

    impl Read for AlignedWriteDisk {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl Write for AlignedWriteDisk {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let offset = self.inner.position();

            if offset % self.alignment != 0 || buf.len() as u64 % self.alignment != 0 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("unaligned write offset={offset} length={}", buf.len()),
                ));
            }

            self.inner.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for AlignedWriteDisk {
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
