use super::*;
use anyhow::{Context, Result};
use iced::widget::{
    button, checkbox, column, container, pick_list, progress_bar, row, scrollable, text,
};
use iced::{time, Alignment, Element, Length, Subscription, Task, Theme};
use serde_json::Value;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
struct DiskInfo {
    number: u32,
    friendly_name: String,
    size: u64,
    bus_type: String,
    is_boot: bool,
    is_system: bool,
    is_offline: bool,
    is_read_only: bool,
    has_offline_partition: bool,
    operational_status: String,
    mount_points: Vec<String>,
}

impl DiskInfo {
    fn is_removable(&self) -> bool {
        matches!(
            self.bus_type.to_ascii_uppercase().as_str(),
            "USB" | "SD" | "MMC" | "1394"
        )
    }

    fn label(&self) -> String {
        let recommendation = if self.is_boot || self.is_system {
            "Unavailable"
        } else if self.is_removable() {
            "Recommended"
        } else {
            "Use with caution"
        };
        format!(
            "{recommendation} — Disk {} · {} · {} · {}",
            self.number,
            self.friendly_name,
            format_bytes(self.size),
            self.bus_type
        )
    }

    fn details(&self) -> String {
        let mount_points = if self.mount_points.is_empty() {
            "none".to_owned()
        } else {
            self.mount_points
                .iter()
                .map(|mount_point| display_mount_point(mount_point))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "PhysicalDrive{} · {} · {} · mounted volumes: {} · {} · {} · status: {}{}",
            self.number,
            self.friendly_name,
            format_bytes(self.size),
            mount_points,
            if self.is_offline { "offline" } else { "online" },
            if self.is_read_only {
                "read-only"
            } else {
                "writable"
            },
            self.operational_status,
            if self.is_boot || self.is_system {
                " · BOOT/SYSTEM DISK — never selectable for writing"
            } else {
                ""
            }
        )
    }
}

#[derive(Clone, Debug)]
struct SourceSelection {
    path: PathBuf,
    kind: Option<ArchiveKind>,
    entries: Vec<ArchiveEntryInfo>,
    selected_entry: Option<String>,
    image_size: Option<u64>,
}

impl SourceSelection {
    fn inspect(path: PathBuf, mode: ArchiveInputMode) -> Result<Self> {
        let kind = match mode {
            ArchiveInputMode::No => None,
            ArchiveInputMode::Auto => archive_kind_from_path(&path),
            ArchiveInputMode::Yes => Some(archive_kind_from_path(&path).with_context(|| {
                format!(
                    "Could not determine archive type from {:?}; supported extensions are {}",
                    path,
                    supported_archive_extensions()
                )
            })?),
        };

        let (entries, image_size) = match kind {
            None => (
                Vec::new(),
                Some(
                    std::fs::metadata(&path)
                        .with_context(|| format!("Could not stat image file {:?}", path))?
                        .len(),
                ),
            ),
            Some(ArchiveKind::Zip) => (archive_candidates(list_zip_entries(&path)?), None),
            Some(ArchiveKind::SevenZ) => (archive_candidates(list_sevenz_entries(&path)?), None),
            Some(ArchiveKind::Tar) => (
                archive_candidates(list_tar_entries(File::open(&path)?)?),
                None,
            ),
            Some(ArchiveKind::TarGz) => (
                archive_candidates(list_tar_entries(GzDecoder::new(File::open(&path)?))?),
                None,
            ),
            Some(ArchiveKind::TarXz) => (
                archive_candidates(list_tar_entries(XzReader::new(File::open(&path)?, true))?),
                None,
            ),
            Some(ArchiveKind::Gzip) | Some(ArchiveKind::Xz) => (Vec::new(), None),
        };

        let selected_entry = (entries.len() == 1).then(|| entries[0].path.clone());
        let image_size = selected_entry
            .as_ref()
            .and_then(|selected| entries.iter().find(|entry| &entry.path == selected))
            .map(|entry| entry.size)
            .or(image_size);

        Ok(Self {
            path,
            kind,
            entries,
            selected_entry,
            image_size,
        })
    }

    fn description(&self) -> String {
        match self.kind {
            None => format!(
                "Raw image · {}",
                self.image_size
                    .map(format_bytes)
                    .unwrap_or_else(|| "size unavailable".to_owned())
            ),
            Some(ArchiveKind::Gzip) | Some(ArchiveKind::Xz) => format!(
                "{} compressed stream · expanded size is unknown until decompression completes",
                self.kind.expect("archive kind is set").label()
            ),
            Some(kind) => match &self.selected_entry {
                Some(entry) => format!(
                    "{} archive · {} · {}",
                    kind.label(),
                    entry,
                    self.image_size
                        .map(format_bytes)
                        .unwrap_or_else(|| "size unavailable".to_owned())
                ),
                None => format!("{} archive · choose an image entry", kind.label()),
            },
        }
    }
}

#[derive(Clone)]
struct GuiSyncRequest {
    source: SourceSelection,
    disk: DiskInfo,
    options: SyncOptions,
    cancel: Arc<AtomicBool>,
}

enum WorkerEvent {
    Started { total: Option<u64>, label: String },
    Sync(SyncEvent),
    Manual(ManualTestEvent),
    Finished(Result<JobResult, String>),
}

enum JobResult {
    Sync(SyncSummary),
    Manual(ManualTestSummary),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupPhase {
    Pending,
    CheckingAdministrator,
    DiscoveringDisks,
    Ready,
}

struct GuiState {
    source: Option<SourceSelection>,
    disks: Vec<DiskInfo>,
    selected_disk: Option<usize>,
    archive_mode: ArchiveInputMode,
    block_size_mib: u64,
    write_mode: bool,
    verify_writes: bool,
    manual_test: bool,
    confirmed: bool,
    is_admin: bool,
    startup: StartupPhase,
    refreshing_disks: bool,
    choosing_image: bool,
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    receiver: Option<mpsc::Receiver<WorkerEvent>>,
    started: Option<Instant>,
    progress_total: Option<u64>,
    progress: f32,
    progress_details: String,
    notice: String,
    notice_error: bool,
}

impl GuiState {
    fn new() -> Self {
        Self {
            source: None,
            disks: Vec::new(),
            selected_disk: None,
            archive_mode: ArchiveInputMode::Auto,
            block_size_mib: 4,
            write_mode: false,
            verify_writes: true,
            manual_test: false,
            confirmed: false,
            is_admin: false,
            startup: StartupPhase::Pending,
            refreshing_disks: false,
            choosing_image: false,
            running: false,
            cancel: None,
            receiver: None,
            started: None,
            progress_total: None,
            progress: 0.0,
            progress_details: "No operation running.".to_owned(),
            notice: String::new(),
            notice_error: false,
        }
    }

    fn selected_disk(&self) -> Option<&DiskInfo> {
        self.selected_disk.and_then(|index| self.disks.get(index))
    }
}

#[derive(Clone, Debug)]
enum Message {
    BeginStartup,
    AdministratorChecked(bool),
    DisksDiscovered(Result<Vec<DiskInfo>, String>),
    DisksRefreshed(Result<Vec<DiskInfo>, String>),
    ChooseImage,
    ImageChosen(Option<PathBuf>),
    RefreshDisks,
    SelectDisk(DiskChoice),
    SelectArchiveEntry(String),
    SelectArchiveMode(ArchiveModeChoice),
    SelectBlockSize(BlockSizeChoice),
    SetWriteMode(bool),
    SetVerifyWrites(bool),
    SetManualTest(bool),
    SetConfirmed(bool),
    RestartAsAdministrator,
    StartJob,
    StopJob,
    Tick,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiskChoice {
    number: u32,
    label: String,
}

impl std::fmt::Display for DiskChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.label.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveModeChoice {
    Auto,
    RawImage,
    Archive,
}

impl ArchiveModeChoice {
    const ALL: [Self; 3] = [Self::Auto, Self::RawImage, Self::Archive];
}

impl From<ArchiveInputMode> for ArchiveModeChoice {
    fn from(mode: ArchiveInputMode) -> Self {
        match mode {
            ArchiveInputMode::Auto => Self::Auto,
            ArchiveInputMode::No => Self::RawImage,
            ArchiveInputMode::Yes => Self::Archive,
        }
    }
}

impl From<ArchiveModeChoice> for ArchiveInputMode {
    fn from(mode: ArchiveModeChoice) -> Self {
        match mode {
            ArchiveModeChoice::Auto => Self::Auto,
            ArchiveModeChoice::RawImage => Self::No,
            ArchiveModeChoice::Archive => Self::Yes,
        }
    }
}

impl std::fmt::Display for ArchiveModeChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => "Auto-detect",
            Self::RawImage => "Raw image",
            Self::Archive => "Archive",
        }
        .fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockSizeChoice {
    Small,
    Balanced,
    Large,
}

impl BlockSizeChoice {
    const ALL: [Self; 3] = [Self::Small, Self::Balanced, Self::Large];

    fn mib(self) -> u64 {
        match self {
            Self::Small => 1,
            Self::Balanced => 4,
            Self::Large => 16,
        }
    }
}

impl From<u64> for BlockSizeChoice {
    fn from(mib: u64) -> Self {
        match mib {
            1 => Self::Small,
            16 => Self::Large,
            _ => Self::Balanced,
        }
    }
}

impl std::fmt::Display for BlockSizeChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Small => "1 MiB — smaller writes",
            Self::Balanced => "4 MiB — balanced",
            Self::Large => "16 MiB — faster",
        }
        .fmt(formatter)
    }
}

struct BimSyncApp {
    state: GuiState,
}

impl BimSyncApp {
    fn new() -> Self {
        Self {
            state: GuiState::new(),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BeginStartup => {
                if self.state.startup != StartupPhase::Pending {
                    return Task::none();
                }
                self.state.startup = StartupPhase::CheckingAdministrator;
                return Task::perform(async { is_administrator() }, Message::AdministratorChecked);
            }
            Message::AdministratorChecked(is_admin) => {
                self.state.is_admin = is_admin;
                self.state.startup = StartupPhase::DiscoveringDisks;
                return Task::perform(discover_disks_async(), Message::DisksDiscovered);
            }
            Message::DisksDiscovered(result) => {
                self.state.startup = StartupPhase::Ready;
                apply_discovered_disks(&mut self.state, result);
            }
            Message::DisksRefreshed(result) => {
                self.state.refreshing_disks = false;
                apply_discovered_disks(&mut self.state, result);
            }
            Message::ChooseImage => {
                if self.state.choosing_image {
                    return Task::none();
                }
                self.state.choosing_image = true;
                return Task::perform(pick_image(), Message::ImageChosen);
            }
            Message::ImageChosen(path) => {
                self.state.choosing_image = false;
                choose_image(&mut self.state, path);
            }
            Message::RefreshDisks => {
                if self.state.refreshing_disks {
                    return Task::none();
                }
                self.state.refreshing_disks = true;
                return Task::perform(discover_disks_async(), Message::DisksRefreshed);
            }
            Message::SelectDisk(choice) => {
                self.state.selected_disk = self
                    .state
                    .disks
                    .iter()
                    .position(|disk| disk.number == choice.number);
                self.state.confirmed = false;
            }
            Message::SelectArchiveEntry(path) => select_archive_entry(&mut self.state, &path),
            Message::SelectArchiveMode(mode) => set_archive_mode(&mut self.state, mode.into()),
            Message::SelectBlockSize(choice) => self.state.block_size_mib = choice.mib(),
            Message::SetWriteMode(value) => {
                self.state.write_mode = value;
                self.state.confirmed = false;
            }
            Message::SetVerifyWrites(value) => self.state.verify_writes = value,
            Message::SetManualTest(value) => {
                self.state.manual_test = value;
                self.state.confirmed = false;
            }
            Message::SetConfirmed(value) => self.state.confirmed = value,
            Message::RestartAsAdministrator => match restart_as_administrator() {
                Ok(()) => {
                    self.state.notice =
                        "An elevated BIM Sync window is opening. Close this window.".to_owned();
                    self.state.notice_error = false;
                }
                Err(error) => {
                    self.state.notice = error.to_string();
                    self.state.notice_error = true;
                }
            },
            Message::StartJob => launch_job(&mut self.state),
            Message::StopJob => stop_job(&mut self.state),
            Message::Tick => poll_worker(&mut self.state),
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let startup = if self.state.startup == StartupPhase::Pending {
            time::every(Duration::from_millis(10)).map(|_| Message::BeginStartup)
        } else {
            Subscription::none()
        };
        let worker = if self.state.receiver.is_some() {
            time::every(Duration::from_millis(100)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        Subscription::batch([startup, worker])
    }

    fn view(&self) -> Element<'_, Message> {
        if self.state.startup != StartupPhase::Ready {
            return startup_view(self.state.startup);
        }

        let disk_choices: Vec<_> = self
            .state
            .disks
            .iter()
            .map(|disk| DiskChoice {
                number: disk.number,
                label: disk.label(),
            })
            .collect();
        let selected_disk = self.state.selected_disk().map(|disk| DiskChoice {
            number: disk.number,
            label: disk.label(),
        });
        let disk_details = self
            .state
            .selected_disk()
            .map(DiskInfo::details)
            .unwrap_or_else(|| "No physical disk selected. Recommended disks are removable USB, SD, or MMC media; choose one deliberately.".to_owned());

        let (image_path, image_details, archive_entries, selected_entry) = self
            .state
            .source
            .as_ref()
            .map(|source| {
                (
                    source.path.to_string_lossy().into_owned(),
                    source.description(),
                    source
                        .entries
                        .iter()
                        .map(|entry| format!("{} ({})", entry.path, format_bytes(entry.size)))
                        .collect::<Vec<_>>(),
                    source.selected_entry.as_ref().and_then(|selected| {
                        source
                            .entries
                            .iter()
                            .find(|entry| &entry.path == selected)
                            .map(|entry| format!("{} ({})", entry.path, format_bytes(entry.size)))
                    }),
                )
            })
            .unwrap_or_else(|| {
                (
                    "No image selected".to_owned(),
                    "Choose a raw image or supported archive.".to_owned(),
                    Vec::new(),
                    None,
                )
            });

        let mut image_button = button(if self.state.choosing_image {
            "Opening image picker…"
        } else {
            "Choose image…"
        });
        if !self.state.running && !self.state.choosing_image {
            image_button = image_button.on_press(Message::ChooseImage);
        }
        let archive_entry_control: Element<'_, Message> = if archive_entries.len() > 1 {
            column![
                text("Archive entry"),
                pick_list(archive_entries, selected_entry, Message::SelectArchiveEntry)
                    .width(Length::Fill)
            ]
            .spacing(4)
            .into()
        } else {
            container(text("")).into()
        };

        let image_section = section(
            "1. Image",
            column![
                row![
                    container(text(image_path).wrapping(text::Wrapping::Word)).width(Length::Fill),
                    image_button
                ]
                .spacing(10)
                .align_y(Alignment::Center),
                text(image_details).size(14),
                row![
                    text("Input type"),
                    pick_list(
                        ArchiveModeChoice::ALL.to_vec(),
                        Some(ArchiveModeChoice::from(self.state.archive_mode)),
                        Message::SelectArchiveMode
                    )
                    .width(220)
                ]
                .spacing(10)
                .align_y(Alignment::Center),
                archive_entry_control,
            ]
            .spacing(7),
        );

        let mut refresh_button = button(if self.state.refreshing_disks {
            "Refreshing…"
        } else {
            "Refresh"
        });
        if !self.state.running && !self.state.refreshing_disks {
            refresh_button = refresh_button.on_press(Message::RefreshDisks);
        }
        let target_section = section(
            "2. Target disk",
            column![
                row![
                    pick_list(disk_choices, selected_disk, Message::SelectDisk)
                        .placeholder("Select a target disk")
                        .width(Length::Fill),
                    refresh_button
                ]
                .spacing(10)
                .align_y(Alignment::Center),
                text(disk_details).size(14),
            ]
            .spacing(7),
        );

        let operation_section = section(
            "3. Operation",
            column![
                checkbox(self.state.write_mode)
                    .label("Write changed blocks (otherwise compare only)")
                    .on_toggle(Message::SetWriteMode),
                checkbox(self.state.verify_writes)
                    .label("Verify every block after writing (recommended)")
                    .on_toggle(Message::SetVerifyWrites),
                checkbox(self.state.manual_test)
                    .label("Run destructive two-block diagnostic on a disposable removable card")
                    .on_toggle(Message::SetManualTest),
                checkbox(self.state.confirmed)
                    .label("I have verified the selected target disk")
                    .on_toggle(Message::SetConfirmed),
                row![
                    text("Block size"),
                    pick_list(
                        BlockSizeChoice::ALL.to_vec(),
                        Some(BlockSizeChoice::from(self.state.block_size_mib)),
                        Message::SelectBlockSize
                    )
                    .width(240),
                    administrator_status(self.state.is_admin)
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            ]
            .spacing(7),
        );

        let readiness = readiness(&self.state);
        let readiness_prefix = if readiness.blocking {
            "Action required: "
        } else {
            ""
        };
        let readiness_section = section(
            "Readiness",
            text(format!("{readiness_prefix}{}", readiness.message)).size(14),
        );

        let action_label = if self.state.manual_test {
            "Run destructive diagnostic"
        } else if self.state.write_mode {
            "Sync changed blocks"
        } else {
            "Compare selected disk"
        };
        let mut start_button = button(action_label);
        if !self.state.running && readiness.can_start {
            start_button = start_button.on_press(Message::StartJob);
        }
        let mut stop_button = button("Stop safely");
        if self.state.running && !self.state.manual_test && self.state.cancel.is_some() {
            stop_button = stop_button.on_press(Message::StopJob);
        }
        let progress_section = section(
            "Progress",
            column![
                progress_bar(0.0..=1.0, self.state.progress).girth(8),
                text(&self.state.progress_details).size(14),
                row![start_button, stop_button].spacing(10),
            ]
            .spacing(7),
        );

        let notice: Element<'_, Message> = if self.state.notice.is_empty() {
            container(text("")).into()
        } else {
            let prefix = if self.state.notice_error {
                "Error: "
            } else {
                ""
            };
            container(text(format!("{prefix}{}", self.state.notice)).size(14))
                .padding([4, 0])
                .into()
        };

        let content = column![
            text("BIM Sync").size(32),
            text("Incrementally compare and sync raw disk images without rewriting unchanged blocks.")
                .size(15),
            image_section,
            target_section,
            operation_section,
            readiness_section,
            progress_section,
            notice,
        ]
        .spacing(14)
        .padding(14)
        .width(Length::Fill);

        scrollable(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

fn startup_view(phase: StartupPhase) -> Element<'static, Message> {
    let status = match phase {
        StartupPhase::Pending => "Starting BIM Sync…",
        StartupPhase::CheckingAdministrator => "Checking administrator permissions…",
        StartupPhase::DiscoveringDisks => "Discovering physical disks…",
        StartupPhase::Ready => "Ready.",
    };
    container(
        column![
            text("BIM Sync").size(26),
            text(status).size(16),
            text("Please wait while Windows is queried for the available target disks.").size(14),
        ]
        .spacing(8)
        .width(440),
    )
    .padding(24)
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .into()
}

fn administrator_status(is_admin: bool) -> Element<'static, Message> {
    if is_admin {
        text("Administrator: ready").size(14).into()
    } else {
        button("Restart as administrator")
            .on_press(Message::RestartAsAdministrator)
            .into()
    }
}

fn section<'a>(title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(column![text(title).size(18), body.into()].spacing(6))
        .padding(10)
        .width(Length::Fill)
        .into()
}

struct Readiness {
    message: String,
    blocking: bool,
    can_start: bool,
}

fn readiness(state: &GuiState) -> Readiness {
    let Some(disk) = state.selected_disk() else {
        return Readiness {
            message: "Choose a physical target disk. The GUI intentionally never selects one automatically.".to_owned(),
            blocking: true,
            can_start: false,
        };
    };
    if !state.is_admin {
        return Readiness {
            message: "Administrator rights are required to inspect and access raw physical disks. Restart the GUI as administrator.".to_owned(),
            blocking: true,
            can_start: false,
        };
    }
    if disk.is_boot || disk.is_system {
        return Readiness {
            message: "This is a Windows boot or system disk. BIM Sync will not operate on it."
                .to_owned(),
            blocking: true,
            can_start: false,
        };
    }
    if state.manual_test && !disk.is_removable() {
        return Readiness {
            message: "The destructive diagnostic is restricted to removable USB, SD, or MMC media."
                .to_owned(),
            blocking: true,
            can_start: false,
        };
    }
    if !state.manual_test {
        let Some(source) = state.source.as_ref() else {
            return Readiness {
                message: "Choose the image to compare or sync.".to_owned(),
                blocking: true,
                can_start: false,
            };
        };
        if source.kind.is_some()
            && !matches!(source.kind, Some(ArchiveKind::Gzip | ArchiveKind::Xz))
            && source.selected_entry.is_none()
        {
            return Readiness {
                message: "Choose the image entry inside the archive.".to_owned(),
                blocking: true,
                can_start: false,
            };
        }
        if source.image_size.is_some_and(|size| size > disk.size) {
            return Readiness {
                message: format!(
                    "The selected image ({}) is larger than Disk {} ({}).",
                    format_bytes(source.image_size.expect("checked above")),
                    disk.number,
                    format_bytes(disk.size)
                ),
                blocking: true,
                can_start: false,
            };
        }
        if image_is_on_target(source, disk) {
            return Readiness {
                message: "The image file is stored on the selected target disk. Choose another source location before writing the target.".to_owned(),
                blocking: true,
                can_start: false,
            };
        }
    }
    if (state.write_mode || state.manual_test) && !state.confirmed {
        return Readiness {
            message:
                "Confirm that the selected drive is the intended disposable target before writing."
                    .to_owned(),
            blocking: true,
            can_start: false,
        };
    }
    if state.write_mode || state.manual_test {
        if disk.is_read_only {
            return Readiness {
                message: "The selected disk is read-only. Clear the read-only state in Windows before writing."
                    .to_owned(),
                blocking: true,
                can_start: false,
            };
        }
        if disk.is_removable() && disk.has_offline_partition {
            return Readiness {
                message: "A target partition is offline, likely left by an older BIM Sync version. Bring it online in Disk Management or reinsert the card before writing."
                    .to_owned(),
                blocking: true,
                can_start: false,
            };
        }
    }
    let message = if state.manual_test {
        format!(
            "Ready to run the destructive two-block diagnostic on Disk {}. It overwrites the start of the card.",
            disk.number
        )
    } else if state.write_mode {
        format!(
            "Ready to incrementally sync the image to Disk {} with {} verification. Mounted target volumes are locked only while BIM Sync writes.",
            disk.number,
            if state.verify_writes {
                "read-after-write"
            } else {
                "no"
            }
        )
    } else {
        format!(
            "Ready to compare the image with Disk {}. No disk data will be modified.",
            disk.number
        )
    };
    Readiness {
        message,
        blocking: false,
        can_start: true,
    }
}

fn image_is_on_target(source: &SourceSelection, disk: &DiskInfo) -> bool {
    let source = source.path.to_string_lossy().to_ascii_uppercase();
    disk.mount_points
        .iter()
        .map(|mount_point| mount_point.replace('/', "\\").to_ascii_uppercase())
        .map(|mount_point| {
            if mount_point.ends_with('\\') {
                mount_point
            } else {
                format!("{mount_point}\\")
            }
        })
        .any(|mount_point| source.starts_with(&mount_point))
}

fn display_mount_point(mount_point: &str) -> String {
    mount_point.trim_end_matches(['\\', '/']).to_owned()
}

pub fn run() -> Result<()> {
    iced::application(BimSyncApp::new, BimSyncApp::update, BimSyncApp::view)
        .title("BIM Sync")
        .theme(app_theme)
        .subscription(BimSyncApp::subscription)
        .window_size((900.0, 760.0))
        .run()
        .context("Native GUI event loop failed")
}

fn app_theme(_: &BimSyncApp) -> Theme {
    Theme::Dark
}

async fn pick_image() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .add_filter(
            "Images and archives",
            &[
                "img", "raw", "bin", "iso", "wic", "zip", "7z", "tar", "gz", "tgz", "xz", "txz",
            ],
        )
        .pick_file()
        .await
        .map(|file| file.path().to_owned())
}

fn choose_image(state: &mut GuiState, path: Option<PathBuf>) {
    let Some(path) = path else {
        return;
    };

    state.confirmed = false;
    match SourceSelection::inspect(path, state.archive_mode) {
        Ok(source) => {
            state.source = Some(source);
            state.notice.clear();
            state.notice_error = false;
        }
        Err(error) => {
            state.source = None;
            state.notice = error.to_string();
            state.notice_error = true;
        }
    }
}

async fn discover_disks_async() -> Result<Vec<DiskInfo>, String> {
    discover_disks().map_err(|error| error.to_string())
}

fn apply_discovered_disks(state: &mut GuiState, result: Result<Vec<DiskInfo>, String>) {
    match result {
        Ok(disks) => {
            let selected_number = state.selected_disk().map(|disk| disk.number);
            state.disks = disks;
            state.selected_disk = selected_number
                .and_then(|number| state.disks.iter().position(|disk| disk.number == number));
            state.notice.clear();
            state.notice_error = false;
        }
        Err(error) => {
            state.notice = format!("Could not discover physical disks: {error}");
            state.notice_error = true;
        }
    }
}

fn select_archive_entry(state: &mut GuiState, selected: &str) {
    let Some(source) = state.source.as_mut() else {
        return;
    };
    let selected = source.entries.iter().find_map(|entry| {
        (format!("{} ({})", entry.path, format_bytes(entry.size)) == selected)
            .then(|| entry.path.clone())
    });
    source.selected_entry = selected;
    source.image_size = source.selected_entry.as_ref().and_then(|selected| {
        source
            .entries
            .iter()
            .find(|entry| &entry.path == selected)
            .map(|entry| entry.size)
    });
}

fn set_archive_mode(state: &mut GuiState, mode: ArchiveInputMode) {
    let path = state.source.as_ref().map(|source| source.path.clone());
    state.archive_mode = mode;
    state.confirmed = false;
    let Some(path) = path else {
        return;
    };
    match SourceSelection::inspect(path, mode) {
        Ok(source) => {
            state.source = Some(source);
            state.notice.clear();
            state.notice_error = false;
        }
        Err(error) => {
            state.source = None;
            state.notice = error.to_string();
            state.notice_error = true;
        }
    }
}

fn launch_job(state: &mut GuiState) {
    let check = readiness(state);
    if !check.can_start {
        state.notice = check.message;
        state.notice_error = true;
        return;
    }

    let disk = state
        .selected_disk()
        .cloned()
        .expect("readiness requires a selected disk");
    let (sender, receiver) = mpsc::channel();
    state.running = true;
    state.receiver = Some(receiver);
    state.started = Some(Instant::now());
    state.progress = 0.0;
    state.progress_total = None;
    state.progress_details = "Starting…".to_owned();
    state.notice.clear();
    state.notice_error = false;

    if state.manual_test {
        state.cancel = None;
        let block_size = block_size_bytes(state.block_size_mib).expect("validated block size");
        thread::spawn(move || run_manual_job(disk, block_size, sender));
    } else {
        let source = state.source.clone().expect("readiness requires a source");
        let cancel = Arc::new(AtomicBool::new(false));
        state.cancel = Some(Arc::clone(&cancel));
        let options = SyncOptions {
            block_size: block_size_bytes(state.block_size_mib).expect("validated block size"),
            verify_only: !state.write_mode,
            verify_writes: state.verify_writes,
        };
        thread::spawn(move || {
            run_sync_job(
                GuiSyncRequest {
                    source,
                    disk,
                    options,
                    cancel,
                },
                sender,
            )
        });
    }
}

fn stop_job(state: &mut GuiState) {
    let Some(cancel) = state.cancel.as_ref() else {
        return;
    };
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    state.notice = "Stopping after the current block. The target may be partially updated; rerun sync to repair it.".to_owned();
    state.notice_error = true;
}

fn poll_worker(state: &mut GuiState) {
    let events: Vec<_> = state
        .receiver
        .as_ref()
        .map(|receiver| receiver.try_iter().collect())
        .unwrap_or_default();
    for event in events {
        handle_worker_event(state, event);
    }
}

fn handle_worker_event(state: &mut GuiState, event: WorkerEvent) {
    match event {
        WorkerEvent::Started { total, label } => {
            state.progress_total = total;
            state.progress_details = format!("{label}\nWaiting for the first block…");
        }
        WorkerEvent::Sync(event) => handle_sync_event(state, event),
        WorkerEvent::Manual(event) => handle_manual_event(state, event),
        WorkerEvent::Finished(result) => {
            state.running = false;
            state.cancel = None;
            state.receiver = None;
            match result {
                Ok(JobResult::Sync(summary)) => {
                    state.progress = 1.0;
                    state.notice = sync_summary_text(summary);
                    state.notice_error = false;
                    state.progress_details = format!(
                        "Completed in {}\n{}",
                        elapsed(state),
                        sync_summary_text(summary)
                    );
                }
                Ok(JobResult::Manual(summary)) => {
                    state.progress = 1.0;
                    state.notice = format!(
                        "Destructive diagnostic completed. It modified {} bytes at offset {}, repaired the card, and verified the repaired result.",
                        summary.mutation_length, summary.mutation_offset
                    );
                    state.notice_error = false;
                    state.progress_details =
                        format!("Completed in {}\n{}", elapsed(state), state.notice);
                }
                Err(error) => {
                    let stopped = error.contains("Operation stopped by the user");
                    state.notice = if stopped {
                        "Stopped safely at a block boundary. The disk may be partially updated; rerun sync to repair it.".to_owned()
                    } else {
                        format!("Operation failed: {error}")
                    };
                    state.notice_error = true;
                    state.progress_details = format!("{}\n{}", elapsed(state), state.notice);
                }
            }
        }
    }
}

fn handle_sync_event(state: &mut GuiState, event: SyncEvent) {
    match event {
        SyncEvent::Progress {
            checked_bytes,
            differing_bytes,
            rewrite_bytes,
            image_size,
        } => {
            if state.progress_total.is_none() {
                state.progress_total = image_size;
            }
            state.progress = state
                .progress_total
                .filter(|total| *total > 0)
                .map(|total| (checked_bytes as f64 / total as f64).min(1.0) as f32)
                .unwrap_or(0.0);
            let speed = state
                .started
                .and_then(|started| checked_bytes.checked_div(started.elapsed().as_secs().max(1)))
                .unwrap_or(0);
            let eta = state.progress_total.and_then(|total| {
                (speed > 0)
                    .then(|| Duration::from_secs(total.saturating_sub(checked_bytes) / speed))
            });
            let total = state
                .progress_total
                .map(format_bytes)
                .unwrap_or_else(|| "unknown total".to_owned());
            state.progress_details = format!(
                "{} of {} checked · {}/s{}\nExact differences: {} · data requiring rewrite: {}",
                format_bytes(checked_bytes),
                total,
                format_bytes(speed),
                eta.map(|eta| format!(" · about {} left", format_duration(eta)))
                    .unwrap_or_default(),
                format_bytes(differing_bytes),
                format_bytes(rewrite_bytes)
            );
        }
        SyncEvent::Diff { offset, length } => {
            state.notice = format!(
                "Difference found at {} ({} block).",
                format_bytes(offset),
                format_bytes(length as u64)
            );
            state.notice_error = false;
        }
        SyncEvent::Wrote {
            offset,
            length,
            verified,
        } => {
            state.notice = format!(
                "{} {} at {}.",
                if verified {
                    "Wrote and verified"
                } else {
                    "Wrote"
                },
                format_bytes(length as u64),
                format_bytes(offset)
            );
            state.notice_error = false;
        }
    }
}

fn handle_manual_event(state: &mut GuiState, event: ManualTestEvent) {
    match event {
        ManualTestEvent::PhaseStarted(phase) => {
            state.progress = 0.0;
            state.progress_details = format!("Diagnostic: {}", phase.label());
        }
        ManualTestEvent::Sync { event, .. } => handle_sync_event(state, event),
        ManualTestEvent::PhaseSummary { phase, summary } => {
            state.notice = format!("{} complete: {}", phase.label(), sync_summary_text(summary));
            state.notice_error = false;
        }
        ManualTestEvent::Modified { offset, length } => {
            state.notice = format!(
                "Diagnostic changed {} at {} before testing repair.",
                format_bytes(length as u64),
                format_bytes(offset)
            );
            state.notice_error = false;
        }
        ManualTestEvent::PhaseCompleted(phase) => {
            state.notice = format!("Diagnostic {} complete.", phase.label());
            state.notice_error = false;
        }
    }
}

fn run_sync_job(request: GuiSyncRequest, sender: mpsc::Sender<WorkerEvent>) {
    let label = if request.options.verify_only {
        format!("Comparing image with Disk {}", request.disk.number)
    } else {
        format!("Syncing image to Disk {}", request.disk.number)
    };
    let _ = sender.send(WorkerEvent::Started {
        total: request.source.image_size,
        label,
    });
    let result = execute_sync(&request, |event| {
        let _ = sender.send(WorkerEvent::Sync(event));
    })
    .map(JobResult::Sync)
    .map_err(|error| format_error_chain(&error));
    let _ = sender.send(WorkerEvent::Finished(result));
}

fn run_manual_job(disk: DiskInfo, block_size: u64, sender: mpsc::Sender<WorkerEvent>) {
    let test_image_size = match manual_test_image_size(block_size) {
        Ok(size) => size,
        Err(error) => {
            let _ = sender.send(WorkerEvent::Finished(Err(error.to_string())));
            return;
        }
    };
    let _ = sender.send(WorkerEvent::Started {
        total: Some(test_image_size as u64),
        label: format!("Running destructive diagnostic on Disk {}", disk.number),
    });
    let result = (|| -> Result<ManualTestSummary> {
        let disk_path = format!(r"\\.\PhysicalDrive{}", disk.number);
        let mut target = open_disk(disk.number, &disk_path, false)?;
        run_manual_sd_test(
            &mut target,
            ManualTestOptions {
                test_image_size,
                block_size,
            },
            |event| {
                let _ = sender.send(WorkerEvent::Manual(event));
            },
        )
    })()
    .map(JobResult::Manual)
    .map_err(|error| format_error_chain(&error));
    let _ = sender.send(WorkerEvent::Finished(result));
}

fn execute_sync<F>(request: &GuiSyncRequest, mut report: F) -> Result<SyncSummary>
where
    F: FnMut(SyncEvent),
{
    let disk_path = format!(r"\\.\PhysicalDrive{}", request.disk.number);
    let mut disk = open_disk(request.disk.number, &disk_path, request.options.verify_only)?;

    match request.source.kind {
        None => {
            let image_size = request
                .source
                .image_size
                .context("Raw image size is unavailable")?;
            let mut image = File::open(&request.source.path)
                .with_context(|| format!("Could not open image file {:?}", request.source.path))?;
            sync_gui_reader(
                &mut image,
                &mut disk,
                Some(image_size),
                request,
                &mut report,
            )
        }
        Some(ArchiveKind::Zip) => sync_zip(&mut disk, request, &mut report),
        Some(ArchiveKind::Tar) => sync_tar(
            &mut disk,
            request,
            File::open(&request.source.path)
                .with_context(|| format!("Could not open archive {:?}", request.source.path))?,
            &mut report,
        ),
        Some(ArchiveKind::TarGz) => sync_tar(
            &mut disk,
            request,
            GzDecoder::new(File::open(&request.source.path)?),
            &mut report,
        ),
        Some(ArchiveKind::TarXz) => sync_tar(
            &mut disk,
            request,
            XzReader::new(File::open(&request.source.path)?, true),
            &mut report,
        ),
        Some(ArchiveKind::Gzip) => {
            let mut image = GzDecoder::new(File::open(&request.source.path)?);
            sync_gui_reader(&mut image, &mut disk, None, request, &mut report)
        }
        Some(ArchiveKind::Xz) => {
            let mut image = XzReader::new(File::open(&request.source.path)?, true);
            sync_gui_reader(&mut image, &mut disk, None, request, &mut report)
        }
        Some(ArchiveKind::SevenZ) => sync_sevenz(&mut disk, request, &mut report),
    }
}

fn open_disk(disk_number: u32, path: &str, verify_only: bool) -> Result<RawTargetDisk> {
    open_raw_target_disk(disk_number, path, !verify_only)
}

fn sync_gui_reader<I, D, F>(
    image: &mut I,
    disk: &mut D,
    image_size: Option<u64>,
    request: &GuiSyncRequest,
    report: F,
) -> Result<SyncSummary>
where
    I: Read + ?Sized,
    D: Read + Write + Seek,
    F: FnMut(SyncEvent),
{
    sync_image_to_disk_stream_ordered_cancellable(
        image,
        disk,
        image_size,
        request.options,
        FirstBlockWriteOrder::Last,
        Some(request.cancel.as_ref()),
        report,
    )
}

fn sync_zip<D, F>(disk: &mut D, request: &GuiSyncRequest, report: F) -> Result<SyncSummary>
where
    D: Read + Write + Seek,
    F: FnMut(SyncEvent),
{
    let selected = selected_archive_entry(request)?;
    let file = File::open(&request.source.path)?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("Could not read ZIP archive {:?}", request.source.path))?;
    let mut entry = archive
        .by_index(selected.index)
        .with_context(|| format!("Could not open ZIP entry {:?}", selected.path))?;
    sync_gui_reader(&mut entry, disk, Some(selected.size), request, report)
}

fn sync_tar<R, D, F>(
    disk: &mut D,
    request: &GuiSyncRequest,
    reader: R,
    report: F,
) -> Result<SyncSummary>
where
    R: Read,
    D: Read + Write + Seek,
    F: FnMut(SyncEvent),
{
    let selected = request
        .source
        .selected_entry
        .as_deref()
        .context("Choose an archive entry before starting")?;
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .with_context(|| format!("Could not read archive {:?}", request.source.path))?;
    for entry in entries {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = normalize_archive_entry_path(&entry.path()?.to_string_lossy());
        if archive_entry_path_matches(&entry_path, selected) {
            let entry_size = entry.size();
            return sync_gui_reader(&mut entry, disk, Some(entry_size), request, report);
        }
    }
    bail!("Archive does not contain selected entry {selected:?}")
}

fn sync_sevenz<D, F>(disk: &mut D, request: &GuiSyncRequest, mut report: F) -> Result<SyncSummary>
where
    D: Read + Write + Seek,
    F: FnMut(SyncEvent),
{
    let selected = selected_archive_entry(request)?;
    let file = File::open(&request.source.path)?;
    let archive_len = file.metadata()?.len();
    let mut archive =
        sevenz_rust::SevenZReader::new(file, archive_len, sevenz_rust::Password::empty())
            .with_context(|| format!("Could not read 7z archive {:?}", request.source.path))?;
    let mut result = None;
    archive
        .for_each_entries(|entry, reader| {
            if entry.is_directory() || !entry.has_stream() {
                return Ok(true);
            }
            if !archive_entry_path_matches(entry.name(), &selected.path) {
                std::io::copy(reader, &mut std::io::sink()).map_err(sevenz_rust::Error::io)?;
                return Ok(true);
            }
            result = Some(
                sync_gui_reader(reader, disk, Some(entry.size()), request, &mut report).map_err(
                    |error| {
                        sevenz_rust::Error::io_msg(
                            std::io::Error::other(error.to_string()),
                            "sync selected 7z entry",
                        )
                    },
                )?,
            );
            Ok(false)
        })
        .with_context(|| format!("Could not stream 7z archive {:?}", request.source.path))?;
    result.context("Archive does not contain selected entry")
}

fn selected_archive_entry(request: &GuiSyncRequest) -> Result<ArchiveEntryInfo> {
    let selected = request
        .source
        .selected_entry
        .as_deref()
        .context("Choose an archive entry before starting")?;
    request
        .source
        .entries
        .iter()
        .find(|entry| archive_entry_path_matches(&entry.path, selected))
        .cloned()
        .context("The selected archive entry is no longer available")
}

fn archive_candidates(entries: Vec<ArchiveEntryInfo>) -> Vec<ArchiveEntryInfo> {
    let image_entries: Vec<_> = entries
        .iter()
        .filter(|entry| is_image_entry_name(&entry.path))
        .cloned()
        .collect();
    if image_entries.is_empty() {
        entries
    } else {
        image_entries
    }
}

fn list_tar_entries<R: Read>(reader: R) -> Result<Vec<ArchiveEntryInfo>> {
    let mut archive = tar::Archive::new(reader);
    archive
        .entries()?
        .enumerate()
        .filter_map(|(index, entry)| match entry {
            Ok(entry) if entry.header().entry_type().is_file() => {
                let path = entry.path().ok()?.to_string_lossy().into_owned();
                Some(Ok(ArchiveEntryInfo {
                    index,
                    path: normalize_archive_entry_path(&path),
                    size: entry.size(),
                }))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error.into())),
        })
        .collect()
}

fn list_sevenz_entries(path: &Path) -> Result<Vec<ArchiveEntryInfo>> {
    let file = File::open(path)?;
    let archive_len = file.metadata()?.len();
    let archive =
        sevenz_rust::SevenZReader::new(file, archive_len, sevenz_rust::Password::empty())?;
    Ok(archive
        .archive()
        .files
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.has_stream() && !entry.is_directory())
        .map(|(index, entry)| ArchiveEntryInfo {
            index,
            path: normalize_archive_entry_path(entry.name()),
            size: entry.size(),
        })
        .collect())
}

fn discover_disks() -> Result<Vec<DiskInfo>> {
    let script = r#"
$disks = @(Get-Disk | ForEach-Object {
    $disk = $_
    $partitions = @(Get-Partition -DiskNumber $disk.Number -ErrorAction SilentlyContinue)
    $mountPoints = @($partitions |
        ForEach-Object {
            $driveLetter = [char]$_.DriveLetter
            $mountedPaths = @($_.AccessPaths | Where-Object { [string]$_ -match '^[A-Za-z]:\\' })

            # Windows reports an unassigned DriveLetter as NUL after mountvol /P.
            # It is not a mount point. Folder mount points remain valid even when
            # the partition has no drive letter, so retain those access paths.
            if ($driveLetter -eq [char]0 -and $mountedPaths.Count -eq 0) { return }

            $mountedPaths | ForEach-Object { [string]$_ }
        })
    [PSCustomObject]@{
        Number = [int]$disk.Number
        FriendlyName = [string]$disk.FriendlyName
        Size = [UInt64]$disk.Size
        BusType = [string]$disk.BusType
        IsBoot = [bool]$disk.IsBoot
        IsSystem = [bool]$disk.IsSystem
        IsOffline = [bool]$disk.IsOffline
        IsReadOnly = [bool]$disk.IsReadOnly
        HasOfflinePartition = [bool](@($partitions | Where-Object IsOffline).Count -gt 0)
        OperationalStatus = [string]($disk.OperationalStatus -join ', ')
        MountPoints = $mountPoints
    }
})
$disks | ConvertTo-Json -Depth 4 -Compress
"#;
    let value: Value = serde_json::from_str(&run_powershell(script)?)
        .context("Windows returned invalid disk information")?;
    let records: Vec<&serde_json::Map<String, Value>> = match &value {
        Value::Array(items) => items.iter().filter_map(Value::as_object).collect(),
        Value::Object(record) => vec![record],
        _ => bail!("Windows did not return a disk list"),
    };
    let mut disks: Vec<_> = records
        .into_iter()
        .map(|record| DiskInfo {
            number: json_u64(record, "Number") as u32,
            friendly_name: json_string(record, "FriendlyName"),
            size: json_u64(record, "Size"),
            bus_type: json_string(record, "BusType"),
            is_boot: json_bool(record, "IsBoot"),
            is_system: json_bool(record, "IsSystem"),
            is_offline: json_bool(record, "IsOffline"),
            is_read_only: json_bool(record, "IsReadOnly"),
            has_offline_partition: json_bool(record, "HasOfflinePartition"),
            operational_status: json_string(record, "OperationalStatus"),
            mount_points: record
                .get("MountPoints")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    disks.sort_by_key(|disk| {
        (
            if disk.is_boot || disk.is_system {
                2
            } else if disk.is_removable() {
                0
            } else {
                1
            },
            disk.number,
        )
    });
    Ok(disks)
}

fn json_u64(record: &serde_json::Map<String, Value>, key: &str) -> u64 {
    record.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn json_bool(record: &serde_json::Map<String, Value>, key: &str) -> bool {
    record.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn json_string(record: &serde_json::Map<String, Value>, key: &str) -> String {
    record
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_owned()
}

fn is_administrator() -> bool {
    run_powershell(
        "([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    )
    .map(|result| result.trim().eq_ignore_ascii_case("true"))
    .unwrap_or(false)
}

fn restart_as_administrator() -> Result<()> {
    let exe = std::env::current_exe().context("Could not locate bim-sync-gui.exe")?;
    let escaped = exe.to_string_lossy().replace('\'', "''");
    powershell_command()
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("Start-Process -FilePath '{escaped}' -Verb RunAs"),
        ])
        .spawn()
        .context("Could not request administrator elevation")?;
    Ok(())
}

fn run_powershell(script: &str) -> Result<String> {
    let output = powershell_command()
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()
        .context("Could not start PowerShell")?;
    if !output.status.success() {
        bail!(
            "PowerShell failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn powershell_command() -> Command {
    let mut command = Command::new("powershell.exe");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn elapsed(state: &GuiState) -> String {
    state
        .started
        .map(|started| format_duration(started.elapsed()))
        .unwrap_or_else(|| "an unknown duration".to_owned())
}

fn sync_summary_text(summary: SyncSummary) -> String {
    format!(
        "{} differing block(s) · {} exact byte differences · {} rewritten · {} skipped",
        summary.different_blocks,
        format_bytes(summary.differing_bytes),
        format_bytes(summary.rewrite_bytes),
        format_bytes(summary.skipped_bytes())
    )
}

fn format_error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\nCaused by: ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk() -> DiskInfo {
        DiskInfo {
            number: 2,
            friendly_name: "USB SD Reader".to_owned(),
            size: 32 * 1024 * 1024,
            bus_type: "USB".to_owned(),
            is_boot: false,
            is_system: false,
            is_offline: false,
            is_read_only: false,
            has_offline_partition: false,
            operational_status: "Online".to_owned(),
            mount_points: Vec::new(),
        }
    }

    fn source(size: u64, path: &str) -> SourceSelection {
        SourceSelection {
            path: PathBuf::from(path),
            kind: None,
            entries: Vec::new(),
            selected_entry: None,
            image_size: Some(size),
        }
    }

    fn state_with(disk: DiskInfo, source: Option<SourceSelection>) -> GuiState {
        GuiState {
            source,
            disks: vec![disk],
            selected_disk: Some(0),
            archive_mode: ArchiveInputMode::Auto,
            block_size_mib: 4,
            write_mode: false,
            verify_writes: true,
            manual_test: false,
            confirmed: false,
            is_admin: true,
            startup: StartupPhase::Ready,
            refreshing_disks: false,
            choosing_image: false,
            running: false,
            cancel: None,
            receiver: None,
            started: None,
            progress_total: None,
            progress: 0.0,
            progress_details: String::new(),
            notice: String::new(),
            notice_error: false,
        }
    }

    #[test]
    fn compare_is_ready_for_a_safe_removable_target() {
        let state = state_with(disk(), Some(source(8 * 1024, r"C:\images\card.img")));

        let readiness = readiness(&state);

        assert!(readiness.can_start);
        assert!(!readiness.blocking);
    }

    #[test]
    fn writing_to_a_mounted_removable_target_uses_scoped_locking() {
        let mut target = disk();
        target.mount_points = vec![r"E:\".to_owned()];
        let mut state = state_with(target, Some(source(8 * 1024, r"C:\images\card.img")));
        state.write_mode = true;
        state.confirmed = true;

        let readiness = readiness(&state);

        assert!(readiness.can_start);
        assert!(readiness.message.contains("locked only"));
    }

    #[test]
    fn writing_to_a_removable_target_with_an_offline_partition_requires_recovery() {
        let mut target = disk();
        target.has_offline_partition = true;
        let mut state = state_with(target, Some(source(8 * 1024, r"C:\images\card.img")));
        state.write_mode = true;
        state.confirmed = true;

        let readiness = readiness(&state);

        assert!(!readiness.can_start);
        assert!(readiness.message.contains("offline"));
    }

    #[test]
    fn boot_disks_are_never_eligible() {
        let mut target = disk();
        target.is_boot = true;
        let state = state_with(target, Some(source(8 * 1024, r"C:\images\card.img")));

        let readiness = readiness(&state);

        assert!(!readiness.can_start);
        assert!(readiness.message.contains("boot or system"));
    }

    #[test]
    fn oversized_image_is_blocked_before_disk_access() {
        let state = state_with(
            disk(),
            Some(source(64 * 1024 * 1024, r"C:\images\card.img")),
        );

        let readiness = readiness(&state);

        assert!(!readiness.can_start);
        assert!(readiness.message.contains("larger"));
    }

    #[test]
    fn image_on_target_volume_is_blocked() {
        let mut target = disk();
        target.mount_points = vec![r"E:\".to_owned()];
        let state = state_with(target, Some(source(8 * 1024, r"E:\images\card.img")));

        let readiness = readiness(&state);

        assert!(!readiness.can_start);
        assert!(readiness.message.contains("stored on the selected target"));
    }

    #[test]
    fn archive_candidates_prefer_image_like_entries() {
        let entries = vec![
            ArchiveEntryInfo {
                index: 0,
                path: "README.txt".to_owned(),
                size: 20,
            },
            ArchiveEntryInfo {
                index: 1,
                path: "images/card.img".to_owned(),
                size: 1024,
            },
        ];

        let candidates = archive_candidates(entries);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, "images/card.img");
    }
}
