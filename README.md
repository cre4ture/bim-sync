# bim-sync

`bim-sync` incrementally syncs a raw disk image file to a Windows physical disk, such as an SD card.

It compares the image and target disk block by block, writes only blocks that differ, and optionally verifies written blocks by reading them back.

This is useful when repeatedly flashing mostly unchanged SD card images and you want to avoid rewriting the entire card every time.

<img width="1421" height="275" alt="image" src="https://github.com/user-attachments/assets/edc35867-c769-427b-b92f-f755077aaf74" />

## Features

- Writes a raw `.img` file to a Windows physical disk
- Streams an image out of `.zip`, `.7z`, `.tar`, `.tar.gz`, `.tgz`,
  `.tar.xz`, `.txz`, `.gz`, or `.xz` input
- Compares before writing
- Writes only changed blocks
- Supports dry-run comparison mode
- Verifies written blocks by default
- Configurable block size
- Shows a progress bar with throughput and ETA
- Includes a destructive manual SD-card test mode
- Intended for SD cards and removable media

## Warning

This tool writes directly to raw disks.

Using the wrong disk number can destroy data on another drive, including your system disk.

Always verify the target disk before writing.

## Requirements

- Windows
- Rust toolchain
- Administrator PowerShell or Administrator terminal
- Close File Explorer windows and applications using the target card before writing

## Project Layout

```text
bim-sync/
|-- Cargo.toml
`-- src/
    |-- main.rs          # CLI and shared sync engine
    |-- gui.rs           # native desktop application
    `-- bin/
        `-- bim-sync-gui.rs
```

## Build

```powershell
cargo build --release --bins
```

The executables will be created at:

```text
.\target\release\bim-sync.exe
.\target\release\bim-sync-gui.exe
```

## Native GUI

`bim-sync-gui.exe` is a native Windows desktop application built with
[Iced](https://iced.rs/). It does not use a browser, web server, or web UI.
Build and run it with:

```powershell
cargo run --release --bin bim-sync-gui
```

Or run the release executable directly:

```powershell
.\target\release\bim-sync-gui.exe
```

The regular `bim-sync.exe` also opens the GUI when launched with no arguments;
all existing command-line invocations continue to use the CLI.

Run the GUI as Administrator. It discovers Windows physical disks and clearly
identifies each disk number, model, capacity, bus type, mounted drive letters,
and online/read-only state. It never auto-selects a target disk.

The GUI provides the following safeguards and conveniences:

- Highlights removable USB, SD, and MMC media as recommended candidates, while
  permanently rejecting boot and system disks.
- Rejects too-small target disks and image files stored on the target itself.
- Locks and dismounts mounted target volumes only for the lifetime of a write,
  while suggesting preparation for read-only media and fixed disks.
- Selects image-like entries from `.zip`, `.7z`, and tar archives, or asks you
  to choose when the archive is ambiguous.
- Starts in compare-only mode; syncing and the destructive diagnostic require
  an explicit confirmation.
- Shows checked bytes, throughput, ETA when available, exact byte differences,
  bytes that must be rewritten, and skipped bytes.
- Allows a sync to stop after the current block. The result is intentionally
  marked as partially updated so it can be repaired by rerunning the sync.

For fixed disks, use **Bring online** after a successful operation. Removable
media is automatically unlocked when the operation ends.

## Find The Correct Disk Number

Run PowerShell as Administrator:

```powershell
Get-CimInstance Win32_DiskDrive | Select-Object DeviceID,Model,Size
Get-Disk | Select-Object Number,FriendlyName,Size,BusType,IsBoot,IsSystem
```

Look carefully for your SD card.

Example:

```text
DeviceID           Model                           Size
--------           -----                           ----
\\.\PHYSICALDRIVE0 CT4000P3SSD8                    4000784417280
\\.\PHYSICALDRIVE1 Mass Storage Device USB Device  127861977600
```

In this example, the SD card is likely:

```text
PhysicalDrive1
```

So the disk number passed to the tool would be:

```text
1
```

Do not use a disk where `IsBoot` or `IsSystem` is `True`.

## Prepare The Target Disk

Before writing to a fixed disk, take the target disk offline:

```powershell
Set-Disk -Number 1 -IsOffline $true
Set-Disk -Number 1 -IsReadOnly $false
```

Replace `1` with the correct disk number.

For removable media, leave the card online and close File Explorer windows and
any programs using it. `bim-sync` locks and dismounts the card's volumes only
while it owns the raw physical-disk handle, then unlocks them when the operation
finishes. Do not use `mountvol /P`: it leaves the existing partition offline,
which is not a valid state for raw imaging.

## Dry-Run Compare

To compare the image with the SD card without writing anything:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.img --disk 1 --verify-only
```

This reports differing blocks, exact byte differences, and how many bytes are
inside the differing blocks. It does not modify the target disk.

During compare or sync, `bim-sync` shows a progress bar with checked bytes,
throughput, ETA, byte differences, and bytes in differing blocks.

## Sync Image To SD Card

To write only changed blocks and verify each written block:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.img --disk 1
```

## Sync Image From Archive

By default, `--archive auto` treats inputs with archive-like extensions as
archives and all other inputs as raw images. Supported archive inputs are:

```text
.zip, .7z, .tar, .tar.gz, .tgz, .tar.xz, .txz, .gz, .xz
```

Examples:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.zip --disk 1
.\target\release\bim-sync.exe --image C:\path\sdcard.tar.gz --disk 1
.\target\release\bim-sync.exe --image C:\path\sdcard.img.xz --disk 1
.\target\release\bim-sync.exe --image C:\path\sdcard.7z --disk 1
```

You can force archive handling or force raw-image handling:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.zip --disk 1 --archive yes
.\target\release\bim-sync.exe --image C:\path\sdcard.img --disk 1 --archive no
```

For `.zip` and `.7z`, `bim-sync` selects the only regular file if the archive
contains one file, otherwise it selects a single image-like entry such as
`.img`, `.raw`, `.bin`, `.iso`, or `.wic`. If the archive is ambiguous, choose
the entry explicitly:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.zip --disk 1 --archive-entry images/sdcard.img
```

For `.tar`, `.tar.gz`, and `.tar.xz`, automatic selection streams the first
image-like file entry. Use `--archive-entry` for archives whose image entry has
another name.

Archive entries and single-file compressed streams are streamed directly into
the block comparison loop. The tool does not write a temporary uncompressed
image file to disk.

## Manual SD-Card Test Mode

Manual test mode writes a generated two-block test image to the beginning of the target disk, verifies it, modifies 32 bytes in one block, verifies that the difference is detected, repairs the disk by syncing the generated image again, and verifies the repaired result.

The generated test image uses two sync blocks. With the default 4 MiB block
size, the test image is 8 MiB.

The 32-byte mutation is written through its containing sync block so the raw
disk write stays aligned for Windows removable media, while the other sync block
remains unchanged.

The summary distinguishes exact byte differences from bytes in differing
blocks. With the default block size, the manual test should report 32 byte
differences, 4194304 bytes in differing blocks, and 4194304 bytes skipped after
the mutation.

This mode is destructive. It overwrites the first two sync blocks of the
selected disk.

Use it only with a disposable SD card:

```powershell
.\target\release\bim-sync.exe --disk 1 --manual-test
```

Manual test mode does not use `--image`. The `--block-size-mib` option still controls the sync block size used during the test.

## Skip Write Verification

By default, changed blocks are verified after writing.

To skip read-after-write verification:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.img --disk 1 --no-verify-writes
```

Skipping verification may be faster, but it is less safe.

## Change Block Size

The default block size is 4 MiB.

To use a larger block size, for example 16 MiB:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.img --disk 1 --block-size-mib 16
```

Larger blocks may improve throughput but can cause more data to be rewritten when only a small part of a block changed.

Smaller blocks may reduce unnecessary writes but increase overhead.

## Bring The Disk Back Online

After writing to a fixed disk:

```powershell
Set-Disk -Number 1 -IsOffline $false
```

Removable-media volumes are unlocked automatically after the operation.

## Example Workflow

```powershell
cargo build --release

Get-CimInstance Win32_DiskDrive | Select-Object DeviceID,Model,Size
Get-Disk | Select-Object Number,FriendlyName,Size,BusType,IsBoot,IsSystem

.\target\release\bim-sync.exe --image C:\images\sdcard.img --disk 1 --verify-only
.\target\release\bim-sync.exe --image C:\images\sdcard.img --disk 1

# For fixed disks, bring the disk online again after writing.
```

## How It Works

For each block:

Before a write, `bim-sync` obtains exclusive locks for all target volumes and
dismounts them. It retains those locks until the raw-disk handle closes, so
Windows cannot remount an old filesystem halfway through the image update.

1. Read a block from the image file, or from the selected archive entry.
2. Read the corresponding block from the target disk.
3. Compare both blocks.
4. If they are identical, skip the block.
5. If they differ, write the image block to the target disk.
6. Read the block back and verify it, unless `--no-verify-writes` is used.

This means the tool still reads the whole image and the corresponding target area, but it avoids unnecessary writes.

During normal sync, changed bytes in the first sync block are buffered and
written last. This keeps the new image partition table off the target until the
rest of the stream has been written, reducing Windows auto-mount races on
removable media.

Progress and summaries report both exact byte differences and bytes in
differing blocks. Writes happen at block granularity, so a block with only a few
different bytes still causes the whole block to be rewritten.

## Limitations

- The target must be at least as large as the image.
- The tool does not resize partitions.
- The tool does not understand filesystems.
- The tool operates on raw bytes only.
- It still reads the full image range.
- It does not currently zero or truncate data beyond the end of the image.
- Windows will refuse a write if another process keeps a target volume open,
  preventing `bim-sync` from obtaining an exclusive lock.
- The tool is Windows-oriented because it targets paths like `\\.\PhysicalDrive1`.
- Archive auto-detection is extension-based. Use `--archive yes` or
  `--archive no` when the extension is misleading.

## When This Is Useful

Good use cases:

- Repeatedly updating a mostly unchanged SD card image
- Reducing SD-card write wear
- Recovering from partially completed image writes
- Verifying whether an SD card already matches an image

Less suitable use cases:

- Syncing individual files
- Updating a mounted filesystem
- Resizing images or partitions
- Copying only used filesystem blocks
- Flashing archives that contain several possible disk images without choosing
  one with `--archive-entry`

## File-Level Alternative

If the SD card is mounted as a normal filesystem and you want to sync files rather than a raw image, use a file-level tool such as `robocopy`, `rsync`, or similar.

This tool is specifically for raw image-to-disk synchronization.

## Safety Checklist

Before writing, confirm:

- The disk number is correct.
- The disk is the SD card.
- The disk is not your boot/system disk.
- Important data on the SD card has been backed up.
- PowerShell or your terminal is running as Administrator.
- File Explorer and other applications are not using the target card.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE).
