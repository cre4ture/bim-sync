# bim-sync

`bim-sync` incrementally syncs a raw disk image file to a Windows physical disk, such as an SD card.

It compares the image and target disk block by block, writes only blocks that differ, and optionally verifies written blocks by reading them back.

This is useful when repeatedly flashing mostly unchanged SD card images and you want to avoid rewriting the entire card every time.

## Features

- Writes a raw `.img` file to a Windows physical disk
- Compares before writing
- Writes only changed blocks
- Supports dry-run comparison mode
- Verifies written blocks by default
- Configurable block size
- Intended for SD cards and removable media

## Warning

This tool writes directly to raw disks.

Using the wrong disk number can destroy data on another drive, including your system disk.

Always verify the target disk before writing.

## Requirements

- Windows
- Rust toolchain
- Administrator PowerShell or Administrator terminal
- Target disk should be offline before writing

## Project Layout

```text
bim-sync/
|-- Cargo.toml
`-- src/
    `-- main.rs
```

## Build

```powershell
cargo build --release
```

The executable will be created at:

```text
.\target\release\bim-sync.exe
```

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

## Take The Target Disk Offline

Before writing, take the SD card offline:

```powershell
Set-Disk -Number 1 -IsOffline $true
Set-Disk -Number 1 -IsReadOnly $false
```

Replace `1` with the correct disk number.

## Dry-Run Compare

To compare the image with the SD card without writing anything:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.img --disk 1 --verify-only
```

This reports differing blocks but does not modify the target disk.

## Sync Image To SD Card

To write only changed blocks and verify each written block:

```powershell
.\target\release\bim-sync.exe --image C:\path\sdcard.img --disk 1
```

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

After writing:

```powershell
Set-Disk -Number 1 -IsOffline $false
```

Windows may then detect the partitions again.

## Example Workflow

```powershell
cargo build --release

Get-CimInstance Win32_DiskDrive | Select-Object DeviceID,Model,Size
Get-Disk | Select-Object Number,FriendlyName,Size,BusType,IsBoot,IsSystem

Set-Disk -Number 1 -IsOffline $true
Set-Disk -Number 1 -IsReadOnly $false

.\target\release\bim-sync.exe --image C:\images\sdcard.img --disk 1 --verify-only
.\target\release\bim-sync.exe --image C:\images\sdcard.img --disk 1

Set-Disk -Number 1 -IsOffline $false
```

## How It Works

For each block:

1. Read a block from the image file.
2. Read the corresponding block from the target disk.
3. Compare both blocks.
4. If they are identical, skip the block.
5. If they differ, write the image block to the target disk.
6. Read the block back and verify it, unless `--no-verify-writes` is used.

This means the tool still reads the whole image and the corresponding target area, but it avoids unnecessary writes.

## Limitations

- The target must be at least as large as the image.
- The tool does not resize partitions.
- The tool does not understand filesystems.
- The tool operates on raw bytes only.
- It still reads the full image range.
- It does not currently zero or truncate data beyond the end of the image.
- Windows may block raw writes if the disk is online or mounted.
- The tool is Windows-oriented because it targets paths like `\\.\PhysicalDrive1`.

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
- Flashing compressed images directly

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
- The target disk has been taken offline.

## License

Choose a license before publishing the project, for example MIT or Apache-2.0.
