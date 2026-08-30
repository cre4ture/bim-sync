use super::*;
use anyhow::{Context, Result};
use serde_json::Value;
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

slint::slint! {
    import { Button, CheckBox, ComboBox, GroupBox, ProgressIndicator, ScrollView, VerticalBox, HorizontalBox } from "std-widgets.slint";

    component CompactButton inherits Button {
        height: 34px;
    }

    component CompactComboBox inherits ComboBox {
        height: 34px;
    }

    component CompactCheckBox inherits CheckBox {
        height: 26px;
    }

    export component MainWindow inherits Window {
        title: "BIM Sync";
        preferred-width: 900px;
        preferred-height: 810px;
        min-width: 720px;
        min-height: 680px;

        in-out property <string> image-path: "";
        in-out property <string> image-details: "Choose a raw image or supported archive.";
        in-out property <[string]> disk-options: ["Select a target disk…"];
        in-out property <int> selected-disk-index: 0;
        in-out property <string> disk-details: "No physical disk selected.";
        in-out property <[string]> archive-entries: [];
        in-out property <int> selected-entry-index: 0;
        in-out property <bool> has-entry-choice: false;
        in-out property <int> archive-mode-index: 0;
        in-out property <int> block-size-index: 1;
        in-out property <bool> write-mode: false;
        in-out property <bool> verify-writes: true;
        in-out property <bool> manual-test: false;
        in-out property <bool> confirmed: false;
        in-out property <bool> running: false;
        in-out property <bool> can-start: false;
        in-out property <bool> can-stop: false;
        in-out property <bool> can-prepare: false;
        in-out property <bool> can-restore: false;
        in-out property <bool> is-admin: false;
        in-out property <string> preflight: "Choose an image and target disk to begin.";
        in-out property <bool> preflight-error: false;
        in-out property <string> notice: "";
        in-out property <bool> notice-error: false;
        in-out property <float> progress: 0;
        in-out property <string> progress-details: "";
        in-out property <string> action-label: "Compare selected disk";

        callback choose-image();
        callback refresh-disks();
        callback select-disk(int);
        callback select-entry(int);
        callback set-archive-mode(int);
        callback set-block-size(int);
        callback set-write-mode(bool);
        callback set-verify-writes(bool);
        callback set-manual-test(bool);
        callback set-confirmed(bool);
        callback prepare-target();
        callback restore-target();
        callback restart-as-admin();
        callback start-job();
        callback stop-job();

        ScrollView {
            mouse-drag-pan-enabled: true;
            VerticalBox {
                padding: 12px;
                spacing: 7px;
                alignment: start;

            Text {
                text: "BIM Sync";
                font-size: 28px;
                font-weight: 700;
            }
            Text {
                text: "Incrementally compare and sync raw disk images without rewriting unchanged blocks.";
                color: #5d6673;
                wrap: word-wrap;
            }

            GroupBox {
                title: "1. Image";
                VerticalBox {
                    spacing: 4px;
                    alignment: start;
                    HorizontalBox {
                        Text {
                            text: root.image-path == "" ? "No image selected" : root.image-path;
                            vertical-alignment: center;
                            overflow: elide;
                        }
                        CompactButton { width: 170px; text: "Choose image…"; clicked => { root.choose-image(); } }
                    }
                    Text { text: root.image-details; color: #5d6673; wrap: word-wrap; }
                    HorizontalBox {
                        Text { text: "Input type"; vertical-alignment: center; }
                        CompactComboBox {
                            model: ["Auto-detect", "Treat as raw image", "Treat as archive"];
                            current-index <=> root.archive-mode-index;
                            selected(value) => { root.set-archive-mode(self.current-index); }
                        }
                        Text { text: "Archive entry"; visible: root.has-entry-choice; vertical-alignment: center; }
                        CompactComboBox {
                            visible: root.has-entry-choice;
                            model: root.archive-entries;
                            current-index <=> root.selected-entry-index;
                            selected(value) => { root.select-entry(self.current-index); }
                        }
                    }
                }
            }

            GroupBox {
                title: "2. Target disk";
                VerticalBox {
                    spacing: 4px;
                    alignment: start;
                    HorizontalBox {
                        CompactComboBox {
                            model: root.disk-options;
                            current-index <=> root.selected-disk-index;
                            enabled: !root.running;
                            selected(value) => { root.select-disk(self.current-index); }
                        }
                        CompactButton { width: 100px; text: "Refresh"; enabled: !root.running; clicked => { root.refresh-disks(); } }
                        CompactButton { width: 130px; text: "Prepare target"; enabled: root.can-prepare; clicked => { root.prepare-target(); } }
                        CompactButton { width: 120px; text: "Bring online"; visible: root.can-restore; enabled: !root.running; clicked => { root.restore-target(); } }
                    }
                    Text { text: root.disk-details; color: #5d6673; wrap: word-wrap; }
                }
            }

            GroupBox {
                title: "3. Operation";
                VerticalBox {
                    spacing: 3px;
                    alignment: start;
                    CompactCheckBox {
                        text: "Write changed blocks (otherwise compare only)";
                        checked: root.write-mode;
                        enabled: !root.running && !root.manual-test;
                        toggled => { root.set-write-mode(self.checked); }
                    }
                    CompactCheckBox {
                        text: "Verify every block after writing (recommended)";
                        checked: root.verify-writes;
                        enabled: !root.running && root.write-mode && !root.manual-test;
                        toggled => { root.set-verify-writes(self.checked); }
                    }
                    CompactCheckBox {
                        text: "Run destructive two-block diagnostic on a disposable removable card";
                        checked: root.manual-test;
                        enabled: !root.running;
                        toggled => { root.set-manual-test(self.checked); }
                    }
                    CompactCheckBox {
                        text: root.manual-test ? "I confirm this is a disposable card" : "I have verified the selected target disk";
                        checked: root.confirmed;
                        visible: root.write-mode || root.manual-test;
                        enabled: !root.running;
                        toggled => { root.set-confirmed(self.checked); }
                    }
                    HorizontalBox {
                        Text { text: "Block size"; vertical-alignment: center; }
                        CompactComboBox {
                            model: ["1 MiB — precise", "4 MiB — balanced", "16 MiB — faster"];
                            current-index <=> root.block-size-index;
                            enabled: !root.running;
                            selected(value) => { root.set-block-size(self.current-index); }
                        }
                        Text {
                            text: root.is-admin ? "Administrator: ready" : "Administrator rights required";
                            color: root.is-admin ? #287a42 : #b45309;
                            vertical-alignment: center;
                        }
                        CompactButton {
                            width: 200px;
                            text: "Restart as administrator";
                            visible: !root.is-admin;
                            enabled: !root.running;
                            clicked => { root.restart-as-admin(); }
                        }
                    }
                }
            }

            GroupBox {
                title: "Readiness";
                Text {
                    text: root.preflight;
                    color: root.preflight-error ? #b42318 : #287a42;
                    wrap: word-wrap;
                }
            }

            GroupBox {
                title: "Progress";
                VerticalBox {
                    spacing: 4px;
                    alignment: start;
                    ProgressIndicator { height: 8px; progress: root.progress; }
                    Text { text: root.progress-details; wrap: word-wrap; color: #374151; }
                    HorizontalBox {
                        CompactButton {
                            width: 190px;
                            text: root.action-label;
                            enabled: root.can-start;
                            clicked => { root.start-job(); }
                        }
                        CompactButton {
                            width: 110px;
                            text: "Stop safely";
                            visible: root.running;
                            enabled: root.can-stop;
                            clicked => { root.stop-job(); }
                        }
                    }
                }
            }

                Text {
                    text: root.notice;
                    color: root.notice-error ? #b42318 : #374151;
                    wrap: word-wrap;
                }
            }
        }
    }
}

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
    Prepared(Result<String, String>),
    Restored(Result<String, String>),
    Finished(Result<JobResult, String>),
}

enum JobResult {
    Sync(SyncSummary),
    Manual(ManualTestSummary),
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
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    receiver: Option<mpsc::Receiver<WorkerEvent>>,
    started: Option<Instant>,
    progress_total: Option<u64>,
    progress: f32,
    progress_details: String,
    notice: String,
    notice_error: bool,
    timer: Timer,
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
            is_admin: is_administrator(),
            running: false,
            cancel: None,
            receiver: None,
            started: None,
            progress_total: None,
            progress: 0.0,
            progress_details: "No operation running.".to_owned(),
            notice: String::new(),
            notice_error: false,
            timer: Timer::default(),
        }
    }

    fn selected_disk(&self) -> Option<&DiskInfo> {
        self.selected_disk.and_then(|index| self.disks.get(index))
    }
}

pub fn run() -> Result<()> {
    let ui = MainWindow::new().context("Could not create the native GUI window")?;
    let state = Rc::new(RefCell::new(GuiState::new()));

    refresh_disks(&state);
    install_callbacks(&ui, &state);
    install_worker_timer(&ui, &state);
    update_ui(&ui, &state.borrow());

    ui.run().context("Native GUI event loop failed")
}

fn install_callbacks(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_choose_image(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        choose_image(&ui, &state_ref);
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_refresh_disks(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        refresh_disks(&state_ref);
        update_ui(&ui, &state_ref.borrow());
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_select_disk(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mut state = state_ref.borrow_mut();
        state.selected_disk = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_sub(1))
            .filter(|index| *index < state.disks.len());
        state.confirmed = false;
        drop(state);
        update_ui(&ui, &state_ref.borrow());
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_select_entry(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(source) = state_ref.borrow_mut().source.as_mut() {
            source.selected_entry = usize::try_from(index)
                .ok()
                .and_then(|index| source.entries.get(index))
                .map(|entry| entry.path.clone());
            source.image_size = source.selected_entry.as_ref().and_then(|selected| {
                source
                    .entries
                    .iter()
                    .find(|entry| &entry.path == selected)
                    .map(|entry| entry.size)
            });
        }
        update_ui(&ui, &state_ref.borrow());
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_set_archive_mode(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mode = match index {
            1 => ArchiveInputMode::No,
            2 => ArchiveInputMode::Yes,
            _ => ArchiveInputMode::Auto,
        };
        let path = state_ref
            .borrow()
            .source
            .as_ref()
            .map(|source| source.path.clone());
        let mut state = state_ref.borrow_mut();
        state.archive_mode = mode;
        state.confirmed = false;
        if let Some(path) = path {
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
        drop(state);
        update_ui(&ui, &state_ref.borrow());
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_set_block_size(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        state_ref.borrow_mut().block_size_mib = match index {
            0 => 1,
            2 => 16,
            _ => 4,
        };
        update_ui(&ui, &state_ref.borrow());
    });

    bind_checkbox_callbacks(ui, state);

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_prepare_target(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        launch_prepare(&ui, &state_ref);
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_restore_target(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        launch_restore(&ui, &state_ref);
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_restart_as_admin(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        restart_as_administrator(&ui, &state_ref);
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_start_job(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        launch_job(&ui, &state_ref);
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_stop_job(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(cancel) = state_ref.borrow().cancel.as_ref() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut state = state_ref.borrow_mut();
            state.notice = "Stopping after the current block. The target may be partially updated; rerun sync to repair it.".to_owned();
            state.notice_error = true;
            drop(state);
            update_ui(&ui, &state_ref.borrow());
        }
    });
}

fn bind_checkbox_callbacks(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_set_write_mode(move |checked| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mut state = state_ref.borrow_mut();
        state.write_mode = checked;
        state.confirmed = false;
        drop(state);
        update_ui(&ui, &state_ref.borrow());
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_set_verify_writes(move |checked| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        state_ref.borrow_mut().verify_writes = checked;
        update_ui(&ui, &state_ref.borrow());
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_set_manual_test(move |checked| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mut state = state_ref.borrow_mut();
        state.manual_test = checked;
        state.confirmed = false;
        drop(state);
        update_ui(&ui, &state_ref.borrow());
    });

    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    ui.on_set_confirmed(move |checked| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        state_ref.borrow_mut().confirmed = checked;
        update_ui(&ui, &state_ref.borrow());
    });
}

fn install_worker_timer(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let weak = ui.as_weak();
    let state_ref = Rc::clone(state);
    state
        .borrow()
        .timer
        .start(TimerMode::Repeated, Duration::from_millis(100), move || {
            if let Some(ui) = weak.upgrade() {
                poll_worker(&ui, &state_ref);
            }
        });
}

fn choose_image(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let path = rfd::FileDialog::new()
        .add_filter(
            "Images and archives",
            &[
                "img", "raw", "bin", "iso", "wic", "zip", "7z", "tar", "gz", "tgz", "xz", "txz",
            ],
        )
        .pick_file();
    let Some(path) = path else {
        return;
    };

    let mode = state.borrow().archive_mode;
    let result = SourceSelection::inspect(path, mode);
    let mut app = state.borrow_mut();
    app.confirmed = false;
    match result {
        Ok(source) => {
            app.source = Some(source);
            app.notice.clear();
            app.notice_error = false;
        }
        Err(error) => {
            app.source = None;
            app.notice = error.to_string();
            app.notice_error = true;
        }
    }
    drop(app);
    update_ui(ui, &state.borrow());
}

fn refresh_disks(state: &Rc<RefCell<GuiState>>) {
    match discover_disks() {
        Ok(disks) => {
            let mut state = state.borrow_mut();
            let selected_number = state.selected_disk().map(|disk| disk.number);
            state.disks = disks;
            state.selected_disk = selected_number
                .and_then(|number| state.disks.iter().position(|disk| disk.number == number));
            state.notice.clear();
            state.notice_error = false;
        }
        Err(error) => {
            let mut state = state.borrow_mut();
            state.notice = format!("Could not discover physical disks: {error}");
            state.notice_error = true;
        }
    }
}

fn update_ui(ui: &MainWindow, state: &GuiState) {
    let disk_options = std::iter::once("Select a target disk…".to_owned())
        .chain(state.disks.iter().map(DiskInfo::label))
        .collect();
    ui.set_disk_options(string_model(disk_options));
    ui.set_selected_disk_index(
        state
            .selected_disk
            .and_then(|index| i32::try_from(index + 1).ok())
            .unwrap_or(0),
    );
    ui.set_disk_details(
        state
            .selected_disk()
            .map(DiskInfo::details)
            .unwrap_or_else(|| "No physical disk selected. Disks marked Recommended are removable USB, SD, or MMC media, but you must choose one deliberately.".to_owned())
            .into(),
    );

    let (path, details, entries, selected_entry) = state
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
                    .collect(),
                source.selected_entry.as_ref().and_then(|selected| {
                    source
                        .entries
                        .iter()
                        .position(|entry| &entry.path == selected)
                }),
            )
        })
        .unwrap_or_else(|| {
            (
                String::new(),
                "Choose a raw image or supported archive.".to_owned(),
                Vec::new(),
                None,
            )
        });
    ui.set_image_path(path.into());
    ui.set_image_details(details.into());
    ui.set_has_entry_choice(entries.len() > 1);
    ui.set_archive_entries(string_model(entries));
    ui.set_selected_entry_index(
        selected_entry
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(0),
    );
    ui.set_archive_mode_index(match state.archive_mode {
        ArchiveInputMode::Auto => 0,
        ArchiveInputMode::No => 1,
        ArchiveInputMode::Yes => 2,
    });
    ui.set_block_size_index(match state.block_size_mib {
        1 => 0,
        16 => 2,
        _ => 1,
    });
    ui.set_write_mode(state.write_mode);
    ui.set_verify_writes(state.verify_writes);
    ui.set_manual_test(state.manual_test);
    ui.set_confirmed(state.confirmed);
    ui.set_running(state.running);
    ui.set_is_admin(state.is_admin);
    ui.set_progress(state.progress);
    ui.set_progress_details(state.progress_details.clone().into());
    ui.set_notice(state.notice.clone().into());
    ui.set_notice_error(state.notice_error);

    let readiness = readiness(state);
    ui.set_preflight(readiness.message.into());
    ui.set_preflight_error(readiness.blocking);
    ui.set_can_start(!state.running && readiness.can_start);
    ui.set_can_stop(state.running && !state.manual_test && state.cancel.is_some());
    ui.set_can_prepare(!state.running && state.is_admin && readiness.can_prepare);
    ui.set_can_restore(
        !state.running
            && state.is_admin
            && state
                .selected_disk()
                .is_some_and(|disk| !disk.is_removable() && disk.is_offline),
    );
    ui.set_action_label(
        if state.manual_test {
            "Run destructive diagnostic"
        } else if state.write_mode {
            "Sync changed blocks"
        } else {
            "Compare selected disk"
        }
        .into(),
    );
}

struct Readiness {
    message: String,
    blocking: bool,
    can_start: bool,
    can_prepare: bool,
}

fn readiness(state: &GuiState) -> Readiness {
    let Some(disk) = state.selected_disk() else {
        return Readiness {
            message: "Choose a physical target disk. The GUI intentionally never selects one automatically.".to_owned(),
            blocking: true,
            can_start: false,
            can_prepare: false,
        };
    };
    if !state.is_admin {
        return Readiness {
            message: "Administrator rights are required to inspect and access raw physical disks. Restart the GUI as administrator.".to_owned(),
            blocking: true,
            can_start: false,
            can_prepare: false,
        };
    }
    if disk.is_boot || disk.is_system {
        return Readiness {
            message: "This is a Windows boot or system disk. BIM Sync will not operate on it."
                .to_owned(),
            blocking: true,
            can_start: false,
            can_prepare: false,
        };
    }
    if state.manual_test && !disk.is_removable() {
        return Readiness {
            message: "The destructive diagnostic is restricted to removable USB, SD, or MMC media."
                .to_owned(),
            blocking: true,
            can_start: false,
            can_prepare: false,
        };
    }
    if !state.manual_test {
        let Some(source) = state.source.as_ref() else {
            return Readiness {
                message: "Choose the image to compare or sync.".to_owned(),
                blocking: true,
                can_start: false,
                can_prepare: false,
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
                can_prepare: false,
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
                can_prepare: false,
            };
        }
        if image_is_on_target(source, disk) {
            return Readiness {
                message: "The image file is stored on the selected target disk. Choose another source location before dismounting or writing the target.".to_owned(),
                blocking: true,
                can_start: false,
                can_prepare: false,
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
            can_prepare: !disk.is_boot && !disk.is_system,
        };
    }
    if state.write_mode || state.manual_test {
        if disk.is_read_only {
            return Readiness {
                message: "The selected disk is read-only. Use Prepare target to clear the read-only state.".to_owned(),
                blocking: true,
                can_start: false,
                can_prepare: true,
            };
        }
        if disk.is_removable() && !disk.mount_points.is_empty() {
            return Readiness {
                message: format!(
                    "Dismount {} before writing. Use Prepare target to dismount mounted removable-media volumes.",
                    disk.mount_points
                        .iter()
                        .map(|mount_point| display_mount_point(mount_point))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                blocking: true,
                can_start: false,
                can_prepare: true,
            };
        }
        if !disk.is_removable() && !disk.is_offline {
            return Readiness {
                message: "Take this fixed disk offline before writing. Use Prepare target."
                    .to_owned(),
                blocking: true,
                can_start: false,
                can_prepare: true,
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
            "Ready to incrementally sync the image to Disk {} with {} verification.",
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
        can_prepare: false,
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

fn launch_prepare(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let Some(disk) = state.borrow().selected_disk().cloned() else {
        return;
    };
    let (sender, receiver) = mpsc::channel();
    {
        let mut state = state.borrow_mut();
        state.running = true;
        state.receiver = Some(receiver);
        state.notice = format!("Preparing Disk {}…", disk.number);
        state.notice_error = false;
    }
    thread::spawn(move || {
        let result = prepare_disk(&disk).map_err(|error| error.to_string());
        let _ = sender.send(WorkerEvent::Prepared(result));
    });
    update_ui(ui, &state.borrow());
}

fn launch_restore(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let Some(disk) = state.borrow().selected_disk().cloned() else {
        return;
    };
    let (sender, receiver) = mpsc::channel();
    {
        let mut state = state.borrow_mut();
        state.running = true;
        state.receiver = Some(receiver);
        state.notice = format!("Bringing Disk {} online…", disk.number);
        state.notice_error = false;
    }
    thread::spawn(move || {
        let result = restore_disk(&disk).map_err(|error| error.to_string());
        let _ = sender.send(WorkerEvent::Restored(result));
    });
    update_ui(ui, &state.borrow());
}

fn launch_job(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let check = readiness(&state.borrow());
    if !check.can_start {
        let mut app = state.borrow_mut();
        app.notice = check.message;
        app.notice_error = true;
        drop(app);
        update_ui(ui, &state.borrow());
        return;
    }

    let disk = state
        .borrow()
        .selected_disk()
        .cloned()
        .expect("readiness requires a selected disk");
    let (sender, receiver) = mpsc::channel();
    let mut state_mut = state.borrow_mut();
    state_mut.running = true;
    state_mut.receiver = Some(receiver);
    state_mut.started = Some(Instant::now());
    state_mut.progress = 0.0;
    state_mut.progress_total = None;
    state_mut.progress_details = "Starting…".to_owned();
    state_mut.notice.clear();
    state_mut.notice_error = false;

    if state_mut.manual_test {
        state_mut.cancel = None;
        let block_size = block_size_bytes(state_mut.block_size_mib).expect("validated block size");
        drop(state_mut);
        thread::spawn(move || run_manual_job(disk, block_size, sender));
    } else {
        let source = state_mut
            .source
            .clone()
            .expect("readiness requires a source");
        let cancel = Arc::new(AtomicBool::new(false));
        state_mut.cancel = Some(Arc::clone(&cancel));
        let options = SyncOptions {
            block_size: block_size_bytes(state_mut.block_size_mib).expect("validated block size"),
            verify_only: !state_mut.write_mode,
            verify_writes: state_mut.verify_writes,
        };
        drop(state_mut);
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
    update_ui(ui, &state.borrow());
}

fn poll_worker(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let events: Vec<_> = state
        .borrow()
        .receiver
        .as_ref()
        .map(|receiver| receiver.try_iter().collect())
        .unwrap_or_default();
    if events.is_empty() {
        return;
    }
    for event in events {
        handle_worker_event(state, event);
    }
    update_ui(ui, &state.borrow());
}

fn handle_worker_event(state: &Rc<RefCell<GuiState>>, event: WorkerEvent) {
    let mut state = state.borrow_mut();
    match event {
        WorkerEvent::Started { total, label } => {
            state.progress_total = total;
            state.progress_details = format!("{label}\nWaiting for the first block…");
        }
        WorkerEvent::Sync(event) => handle_sync_event(&mut state, event),
        WorkerEvent::Manual(event) => handle_manual_event(&mut state, event),
        WorkerEvent::Prepared(result) => {
            state.running = false;
            state.receiver = None;
            match result {
                Ok(message) => {
                    state.notice = message;
                    state.notice_error = false;
                }
                Err(error) => {
                    state.notice = error;
                    state.notice_error = true;
                }
            }
        }
        WorkerEvent::Restored(result) => {
            state.running = false;
            state.receiver = None;
            match result {
                Ok(message) => {
                    state.notice = message;
                    state.notice_error = false;
                }
                Err(error) => {
                    state.notice = error;
                    state.notice_error = true;
                }
            }
        }
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
                        elapsed(&state),
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
                        format!("Completed in {}\n{}", elapsed(&state), state.notice);
                }
                Err(error) => {
                    let stopped = error.contains("Operation stopped by the user");
                    state.notice = if stopped {
                        "Stopped safely at a block boundary. The disk may be partially updated; rerun sync to repair it.".to_owned()
                    } else {
                        format!("Operation failed: {error}")
                    };
                    state.notice_error = true;
                    state.progress_details = format!("{}\n{}", elapsed(&state), state.notice);
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
        let mut target = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk_path)
            .with_context(|| format!("Could not open {disk_path} for read/write"))?;
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
    let mut disk = open_disk(&disk_path, request.options.verify_only)?;

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

fn open_disk(path: &str, verify_only: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    if !verify_only {
        options.write(true);
    }
    options.open(path).with_context(|| {
        if verify_only {
            format!("Could not open {path} for reading")
        } else {
            format!("Could not open {path} for read/write. Prepare the target and run as Administrator.")
        }
    })
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
    $mountPoints = @(Get-Partition -DiskNumber $disk.Number -ErrorAction SilentlyContinue |
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

fn prepare_disk(disk: &DiskInfo) -> Result<String> {
    if disk.is_boot || disk.is_system {
        bail!("Refusing to prepare a Windows boot or system disk")
    }
    let script = if disk.is_removable() {
        format!(
            "$n = {}; Get-Partition -DiskNumber $n -ErrorAction SilentlyContinue | ForEach-Object {{ $mountPoint = @($_.AccessPaths | Where-Object {{ [string]$_ -match '^[A-Za-z]:\\' }} | Select-Object -First 1); if ($mountPoint) {{ mountvol $mountPoint[0] /P; if ($LASTEXITCODE -ne 0) {{ throw \"Could not dismount $($mountPoint[0])\" }} }} }}; Set-Disk -Number $n -IsReadOnly $false -ErrorAction Stop",
            disk.number
        )
    } else {
        format!(
            "$n = {}; Set-Disk -Number $n -IsOffline $true -ErrorAction Stop; Set-Disk -Number $n -IsReadOnly $false -ErrorAction Stop",
            disk.number
        )
    };
    run_powershell(&script)?;
    Ok(if disk.is_removable() {
        format!("Disk {} prepared: mounted volumes were dismounted and read-only mode was cleared. Refresh the disk list before writing.", disk.number)
    } else {
        format!(
            "Disk {} prepared: it is offline and writable. Refresh the disk list before writing.",
            disk.number
        )
    })
}

fn restore_disk(disk: &DiskInfo) -> Result<String> {
    if disk.is_removable() {
        bail!("Removable media cannot reliably have old mount points restored. Reinsert the card after writing.")
    }
    run_powershell(&format!(
        "Set-Disk -Number {} -IsOffline $false -ErrorAction Stop",
        disk.number
    ))?;
    Ok(format!(
        "Disk {} was brought online. Refresh the disk list to confirm its state.",
        disk.number
    ))
}

fn is_administrator() -> bool {
    run_powershell(
        "([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    )
    .map(|result| result.trim().eq_ignore_ascii_case("true"))
    .unwrap_or(false)
}

fn restart_as_administrator(ui: &MainWindow, state: &Rc<RefCell<GuiState>>) {
    let result = (|| -> Result<()> {
        let exe = std::env::current_exe().context("Could not locate bim-sync-gui.exe")?;
        let escaped = exe.to_string_lossy().replace('\'', "''");
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Start-Process -FilePath '{escaped}' -Verb RunAs"),
            ])
            .spawn()
            .context("Could not request administrator elevation")?;
        Ok(())
    })();
    let mut app = state.borrow_mut();
    match result {
        Ok(()) => {
            app.notice =
                "An elevated BIM Sync window is opening. Close this non-administrator window."
                    .to_owned();
            app.notice_error = false;
        }
        Err(error) => {
            app.notice = error.to_string();
            app.notice_error = true;
        }
    }
    drop(app);
    update_ui(ui, &state.borrow());
}

fn run_powershell(script: &str) -> Result<String> {
    let output = Command::new("powershell.exe")
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

fn string_model(values: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )))
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
            running: false,
            cancel: None,
            receiver: None,
            started: None,
            progress_total: None,
            progress: 0.0,
            progress_details: String::new(),
            notice: String::new(),
            notice_error: false,
            timer: Timer::default(),
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
    fn writing_to_a_mounted_removable_target_requires_preparation() {
        let mut target = disk();
        target.mount_points = vec![r"E:\".to_owned()];
        let mut state = state_with(target, Some(source(8 * 1024, r"C:\images\card.img")));
        state.write_mode = true;
        state.confirmed = true;

        let readiness = readiness(&state);

        assert!(!readiness.can_start);
        assert!(readiness.can_prepare);
        assert!(readiness.message.contains("Dismount E:"));
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
