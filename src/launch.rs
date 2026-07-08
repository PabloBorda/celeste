use crate::{
    entities::{
        RemotesColumn, RemotesEntity, RemotesModel, SyncDirsActiveModel, SyncDirsColumn,
        SyncDirsEntity, SyncDirsModel, SyncItemsActiveModel, SyncItemsColumn, SyncItemsEntity,
    },
    gtk_util,
    login::{self},
    migrations::{Migrator, MigratorTrait},
    rclone::{self, RcloneListFilter},
    traits::prelude::*,
    util,
};
use adw::{
    glib,
    gtk::{
        pango::EllipsizeMode, Align, Box, Button, ButtonsType, Entry, EntryCompletion,
        FileChooserDialog, FileFilter, GestureClick, Image, Inhibit, Label, ListBox, ListBoxRow,
        ListStore, MessageDialog, Orientation, PolicyType, Popover, PositionType, ResponseType,
        ProgressBar, ScrolledWindow, SelectionMode, Separator, Spinner, Stack, StackTransitionType,
        Widget,
    },
    prelude::*,
    Application, ApplicationWindow, Bin, EntryRow, HeaderBar, Leaflet, LeafletTransitionType,
    WindowTitle,
};
use file_lock::{FileLock, FileOptions};
use indexmap::IndexMap;
use sea_orm::{entity::prelude::*, ActiveValue, Database, DatabaseConnection};

use std::{
    boxed,
    cell::RefCell,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

// The location for file ignore lists.
static FILE_IGNORE_NAME: &str = ".sync-exclude.lst";

// A [`HashMap`] containing the status and progress for a directory sync label.
// This is done here because if we try to get the child from a `Box` or
// something we just get a generic gtk `Widget`, which we can't use.
type DirectoryMap = Rc<RefCell<IndexMap<String, IndexMap<(String, String), SyncDir>>>>;

// The custom server sidebar rows. GtkStackSidebar only shows the page title, so
// Celeste keeps its own row widgets to show provider, name, and status columns.
type ServerRowMap = Rc<RefCell<IndexMap<String, ServerRow>>>;

// Runtime statistics for one sync directory pass. These values are intentionally
// not persisted: the database already stores durable sync state, while these
// counters exist to explain what the current pass is doing.
type SyncStatsRef = Rc<RefCell<SyncStats>>;

// A [`Vec`] for a deletion queue to remove remotes.
type RemoteDeletionQueue = Rc<RefCell<Vec<String>>>;

// A [`Vec`] for a deletion queue to stop syncing directories - we store this in
// a queue so we can stop syncing directories safely while syncs may still be
// occurring.
type SyncDirDeletionQueue = Rc<RefCell<Vec<(String, String, String)>>>;

/// The errors that can be found while syncing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SyncError {
    /// A general catch-all error. A tuple of the path the error happened at,
    /// and the error message itself.
    General(String, String),
    /// An error when both the local and remote file are more current than at
    /// the last sync. A tuple of the local and remote file.
    BothMoreCurrent(String, String),
}

impl SyncError {
    fn generate_ui(&self) -> Box {
        let error_container = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(2)
            .margin_top(6)
            .margin_end(6)
            .margin_bottom(6)
            .margin_start(6)
            .build();

        match self {
            SyncError::General(file_path, err) => {
                let err_label = Label::builder()
                    .label(file_path)
                    .halign(Align::Start)
                    .ellipsize(EllipsizeMode::End)
                    .build();
                let file_label = Label::builder()
                    .label(err)
                    .halign(Align::Start)
                    .ellipsize(EllipsizeMode::End)
                    .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
                    .build();
                error_container.append(&err_label);
                error_container.append(&file_label);
            }
            SyncError::BothMoreCurrent(local_path, remote_path) => {
                let err_msg = tr::tr!(
                    "Both '{}' and '{}' are more recent than at last sync.",
                    local_path,
                    remote_path
                );
                let err_label = Label::builder()
                    .label(&err_msg)
                    .halign(Align::Start)
                    .ellipsize(EllipsizeMode::End)
                    .build();
                error_container.append(&err_label);
            }
        }

        error_container
    }
}
/// A struct representing all the data that belongs to a sync directory.
struct SyncDir {
    /// The parent stack for [`Self::container`], this contains all the UI
    /// listing for sync directories.
    parent_list: ListBox,
    /// The Box containing things like the progress icon, status text, etc.
    container: ListBoxRow,
    /// The container for the progress icon.
    status_icon: Bin,
    /// The label for reporting errors in the current sync status.
    error_status_text: Label,
    /// The label for reporting the current sync status (things like 'Awaiting
    /// sync check...').
    status_text: Label,
    /// A compact progress bar shown in the directory list.
    progress_bar: ProgressBar,
    /// A compact statistics summary shown in the directory list.
    stats_text: Label,
    /// A larger progress bar shown in the directory details page.
    more_info_progress_bar: ProgressBar,
    /// A statistics summary shown in the directory details page.
    more_info_stats_text: Label,
    /// The current local or remote item being processed, shown in the details page.
    more_info_current_item_text: Label,
    /// The error list in the UI.
    error_list: ListBox,
    /// The list of error items, as generated by 'SyncError::generate_ui' above.
    error_items: HashMap<SyncError, Box>,
    /// A closure to update the UI error listing.
    update_error_ui: boxed::Box<dyn Fn()>,
}

#[derive(Clone)]
struct ServerRow {
    row: ListBoxRow,
    status_icon: Image,
}

#[derive(Clone, Copy)]
enum ServerStatus {
    Checking,
    Working,
    Broken,
}

#[derive(Clone, Debug, Default)]
struct SyncStats {
    known_items: u64,
    local_scanned: u64,
    remote_scanned: u64,
    processed: u64,
    uploaded: u64,
    downloaded: u64,
    deleted: u64,
    skipped: u64,
    errors: u64,
    conflicts: u64,
}

impl SyncStats {
    fn total_hint(&self) -> u64 {
        self.known_items.max(self.local_scanned + self.remote_scanned)
    }

    fn progress_fraction(&self) -> Option<f64> {
        if self.processed == 0 {
            return None;
        }

        let total = self.total_hint();

        if total == 0 {
            None
        } else {
            Some((self.processed as f64 / total as f64).clamp(0.01, 1.0))
        }
    }

    fn progress_text(&self) -> String {
        match self.progress_fraction() {
            Some(fraction) => format!("{:.0}%", fraction * 100.0),
            None if self.local_scanned + self.remote_scanned > 0 => tr::tr!("Scanning"),
            None => tr::tr!("Waiting"),
        }
    }

    fn summary(&self) -> String {
        let total = self.total_hint();
        let processed = if total > 0 {
            tr::tr!("{} / {} processed", self.processed, total)
        } else {
            tr::tr!("{} processed", self.processed)
        };

        tr::tr!(
            "{} • {} local scanned • {} remote scanned • {} uploaded • {} downloaded • {} deleted • {} skipped • {} errors • {} conflicts",
            processed,
            self.local_scanned,
            self.remote_scanned,
            self.uploaded,
            self.downloaded,
            self.deleted,
            self.skipped,
            self.errors,
            self.conflicts
        )
    }
}

lazy_static::lazy_static! {
    // A [`Mutex`] to keep track of any recorded close requests.
    pub static ref CLOSE_REQUEST: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    // A [`Mutex`] to keep track of open requests from the tray icon.
    pub static ref OPEN_REQUEST: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
}

static ADD_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#2ec27e" d="M7 1h2v6h6v2H9v6H7V9H1V7h6z"/></svg>"##;
static REMOVE_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#f66151" d="M2 7h12v2H2z"/></svg>"##;
static TRASH_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#f66151" d="M5 1h6l1 2h3v2H1V3h3zM3 6h10l-.7 9H3.7z"/></svg>"##;
static NEXT_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#99c1f1" d="M5.2 2 11 8l-5.8 6-1.4-1.4L8.2 8 3.8 3.4z"/></svg>"##;
static PREVIOUS_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#99c1f1" d="M10.8 2 5 8l5.8 6 1.4-1.4L7.8 8l4.4-4.6z"/></svg>"##;
static MENU_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#deddda" d="M2 4h12v2H2zm0 3h12v2H2zm0 3h12v2H2z"/></svg>"##;
static FOLDER_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#f6d32d" d="M1 4h5l1.5 2H15v8H1z"/><path fill="#f9f06b" d="M1 3h5l1.5 2H15v2H1z"/></svg>"##;
static DROPBOX_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#1e88e5" d="m6.5 3.5 5.5 3.4-5.5 3.4L1 6.9zm11 0L23 6.9l-5.5 3.4L12 6.9zM1 13.1l5.5-3.4 5.5 3.4-5.5 3.4zm16.5-3.4 5.5 3.4-5.5 3.4-5.5-3.4z"/><path fill="#64b5f6" d="m6.5 17.6 5.5-3.4 5.5 3.4L12 21z"/></svg>"##;
static PCLOUD_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#ef5350" d="M8 18h9.5a4.5 4.5 0 0 0 .4-9 6.5 6.5 0 0 0-12.5-1.8A5.5 5.5 0 0 0 8 18z"/><path fill="#fff" d="M8 8h5a3 3 0 0 1 0 6h-2v3H8zm3 2v2h2a1 1 0 1 0 0-2z"/></svg>"##;
static PROTON_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#6d4aff" d="M8 18h10a5 5 0 0 0 .6-10A7 7 0 0 0 5.1 6.1 5.5 5.5 0 0 0 8 18z"/><path fill="#fff" d="M9 12h8v7H9z"/><path fill="#3d2a9d" d="M11 12v-1a2 2 0 1 1 4 0v1h-1.5v-1a.5.5 0 0 0-1 0v1z"/></svg>"##;
static WEBDAV_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#8ff0a4" d="M3 5h18v14H3z"/><path fill="#1c71d8" d="M5 7h14v3H5z"/><path fill="#fff" d="M6 13h12v2H6z"/></svg>"##;
static CLOUD_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#99c1f1" d="M8 18h10a5 5 0 0 0 .6-10A7 7 0 0 0 5.1 6.1 5.5 5.5 0 0 0 8 18z"/></svg>"##;
static STATUS_OK_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle fill="#2ec27e" cx="8" cy="8" r="7"/><path fill="#fff" d="M6.7 11.3 3.8 8.4l1.2-1.2 1.7 1.7 4.3-4.4 1.2 1.2z"/></svg>"##;
static STATUS_ERROR_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle fill="#e01b24" cx="8" cy="8" r="7"/><path fill="#fff" d="m4.7 5.8 1.1-1.1L8 6.9l2.2-2.2 1.1 1.1L9.1 8l2.2 2.2-1.1 1.1L8 9.1l-2.2 2.2-1.1-1.1L6.9 8z"/></svg>"##;
static STATUS_CHECKING_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle fill="#f6d32d" cx="8" cy="8" r="7"/><path fill="#5e5c64" d="M7.2 3h1.6v6H7.2zm0 8h1.6v2H7.2z"/></svg>"##;

/// Get an icon for use as the status icon for directory syncs.
fn get_image(icon_name: &str) -> Image {
    get_ui_icon(icon_name, 16)
}

fn get_ui_icon(icon_name: &str, size: i32) -> Image {
    let bytes = match icon_name {
        "content-loading-symbolic" => {
            include_bytes!("../assets/context/com.hunterwittenborn.Celeste.CelesteTrayLoading-symbolic.svg").as_slice()
        }
        "object-select-symbolic" => {
            include_bytes!("../assets/context/com.hunterwittenborn.Celeste.CelesteTrayDone-symbolic.svg").as_slice()
        }
        "dialog-warning-symbolic" => {
            include_bytes!("../assets/context/com.hunterwittenborn.Celeste.CelesteTrayWarning-symbolic.svg").as_slice()
        }
        "go-next-symbolic" => NEXT_ICON,
        "go-previous-symbolic" => PREVIOUS_ICON,
        "list-add-symbolic" => ADD_ICON,
        "list-remove-symbolic" => REMOVE_ICON,
        "user-trash-symbolic" => TRASH_ICON,
        "open-menu-symbolic" => MENU_ICON,
        "folder-new" | "folder-symbolic" => FOLDER_ICON,
        _ => include_bytes!("../assets/context/com.hunterwittenborn.Celeste.CelesteTraySyncing-symbolic.svg").as_slice(),
    };

    gtk_util::image_from_bytes(bytes, size, size)
}

fn icon_button(icon_name: &str, size: i32) -> Button {
    Button::builder()
        .child(&get_ui_icon(icon_name, size))
        .build()
}

fn get_sync_logo() -> Image {
    gtk_util::image_from_bytes(include_bytes!("../assets/subjective-rclone-sync.svg"), 42, 28)
}

fn remote_type_icon_bytes(remote_name: &str) -> &'static [u8] {
    let Some(config) = rclone::get_remote_config(remote_name) else {
        return CLOUD_ICON;
    };

    match config.get("type").map(String::as_str) {
        Some("drive") => include_bytes!("images/google-drive.png").as_slice(),
        Some("dropbox") => DROPBOX_ICON,
        Some("pcloud") => PCLOUD_ICON,
        Some("protondrive") => PROTON_ICON,
        Some("webdav") => WEBDAV_ICON,
        _ => CLOUD_ICON,
    }
}

fn server_status_icon_bytes(status: ServerStatus) -> &'static [u8] {
    match status {
        ServerStatus::Checking => STATUS_CHECKING_ICON,
        ServerStatus::Working => STATUS_OK_ICON,
        ServerStatus::Broken => STATUS_ERROR_ICON,
    }
}

fn server_status_tooltip(status: ServerStatus) -> String {
    match status {
        ServerStatus::Checking => tr::tr!("Connection status not checked yet."),
        ServerStatus::Working => tr::tr!("Connection is working."),
        ServerStatus::Broken => tr::tr!("Connection check failed."),
    }
}

fn set_server_status(server_rows: &ServerRowMap, remote_name: &str, status: ServerStatus) {
    let rows = server_rows.borrow();
    let Some(row) = rows.get(remote_name) else {
        return;
    };

    gtk_util::set_image_from_bytes(&row.status_icon, server_status_icon_bytes(status), 16, 16);
    row.status_icon
        .set_tooltip_text(Some(&server_status_tooltip(status)));
}

fn server_name_from_row(row: &ListBoxRow) -> Option<String> {
    let container = row.child().and_downcast::<Box>()?;
    let provider_icon = container.first_child()?;
    let label = provider_icon.next_sibling().and_downcast::<Label>()?;
    Some(label.text().to_string())
}

fn build_server_row(remote_name: &str) -> ServerRow {
    let provider_icon = gtk_util::image_from_bytes(remote_type_icon_bytes(remote_name), 20, 20);
    provider_icon.set_tooltip_text(Some(&tr::tr!("Connection type")));

    let name_label = Label::builder()
        .label(remote_name)
        .xalign(0.0)
        .hexpand(true)
        .hexpand_set(true)
        .ellipsize(EllipsizeMode::End)
        .build();

    let status_icon =
        gtk_util::image_from_bytes(server_status_icon_bytes(ServerStatus::Checking), 16, 16);
    status_icon.set_tooltip_text(Some(&server_status_tooltip(ServerStatus::Checking)));

    let container = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_top(7)
        .margin_end(8)
        .margin_bottom(7)
        .margin_start(8)
        .build();
    container.append(&provider_icon);
    container.append(&name_label);
    container.append(&status_icon);

    let row = ListBoxRow::builder().child(&container).build();
    ServerRow { row, status_icon }
}

fn update_progress_bar(progress_bar: &ProgressBar, stats: &SyncStats) {
    if let Some(fraction) = stats.progress_fraction() {
        progress_bar.set_fraction(fraction);
    } else {
        progress_bar.set_pulse_step(0.05);
        progress_bar.pulse();
    }

    progress_bar.set_text(Some(&stats.progress_text()));
}

fn update_sync_stats_ui(
    directory_map: &DirectoryMap,
    remote_name: &str,
    sync_dir: &SyncDirsModel,
    stats: &SyncStats,
    current_item: Option<&str>,
) {
    let ptr = directory_map.borrow();
    let Some(remote_dirs) = ptr.get(remote_name) else {
        return;
    };
    let dir_pair = (sync_dir.local_path.clone(), sync_dir.remote_path.clone());
    let Some(item) = remote_dirs.get(&dir_pair) else {
        return;
    };

    let summary = stats.summary();
    item.stats_text.set_label(&summary);
    item.more_info_stats_text.set_label(&summary);

    if let Some(current_item) = current_item {
        item.more_info_current_item_text
            .set_label(&tr::tr!("Current item: {}", util::fmt_home(current_item)));
    }

    update_progress_bar(&item.progress_bar, stats);
    update_progress_bar(&item.more_info_progress_bar, stats);
}

fn maybe_process_ui_events(count: u64) {
    if count % 50 == 0 {
        util::process_ui_events();
    }
}

pub fn launch(app: &Application, background: bool) {
    // Create the configuration directory if it doesn't exist.
    let config_path = util::get_config_dir();
    if !config_path.exists()
        && let Err(err) = fs::create_dir_all(&config_path)
    {
        gtk_util::show_error(
            &tr::tr!("Unable to create Celeste's config directory [{}].", err),
            None,
        );
        return;
    }

    // Create the database file if it doesn't exist.
    let mut db_path = config_path;
    db_path.push("celeste.db");
    if !db_path.exists() {
        if let Err(err) = fs::File::create(&db_path) {
            gtk_util::show_error(
                &tr::tr!("Unable to create Celeste's database file [{}].", err),
                None,
            );
            return;
        }
    };

    // Connect to the database.
    let db = util::await_future(Database::connect(format!("sqlite://{}", db_path.display())));
    if let Err(err) = &db {
        gtk_util::show_error(&tr::tr!("Unable to connect to database [{}].", err), None);
        return;
    };
    let db = db.unwrap();

    // Run migrations.
    if let Err(err) = util::await_future(Migrator::up(&db, None)) {
        gtk_util::show_error(
            &tr::tr!("Unable to run database migrations [{}]", err),
            None,
        );
        return;
    }

    // Get our remotes.
    let mut remotes = util::await_future(RemotesEntity::find().all(&db)).unwrap();

    if remotes.is_empty() {
        if login::login(app, &db).is_none() {
            return;
        }

        remotes = util::await_future(RemotesEntity::find().all(&db)).unwrap();
    }

    // Create the main UI.
    let window = ApplicationWindow::builder()
        .application(app)
        .title(&util::get_title!("Servers"))
        .default_height(600)
        .build();
    window.add_css_class("celeste-global-padding");
    let server_rows: ServerRowMap = Rc::new(RefCell::new(IndexMap::new()));
    let server_status_checked: Rc<RefCell<HashMap<String, bool>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let server_list = ListBox::builder()
        .selection_mode(SelectionMode::Single)
        .width_request(220)
        .valign(Align::Start)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    // Wrap the server list in a scrolled window so that adding many
    // connections scrolls the sidebar instead of growing the window height
    // unbounded. We deliberately do NOT propagate the natural height here: if
    // we did, the scrolled window would request enough height to show every
    // row, which both prevents the window from being resized shorter and stops
    // the scrollbar from ever appearing.
    let server_list_scrolled = ScrolledWindow::builder()
        .child(&server_list)
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .min_content_height(200)
        .vexpand(true)
        .vexpand_set(true)
        .build();
    let stack = Stack::new();

    let directory_map: DirectoryMap = Rc::new(RefCell::new(IndexMap::new()));

    // Store any remote deletions (values of the remote names) in a queue so they
    // can be processed when syncing is at a good point of stopping.
    let remote_deletion_queue: RemoteDeletionQueue = Rc::new(RefCell::new(vec![]));

    // Store any sync deletions (the remote + local directory + remote directory) in
    // a queue so they can be processed when syncing is at a good point of stopping.
    let sync_dir_deletion_queue: SyncDirDeletionQueue = Rc::new(RefCell::new(vec![]));

    // Add servers.
    let gen_remote_window = glib::clone!(@strong window, @strong remote_deletion_queue, @strong sync_dir_deletion_queue, @strong directory_map, @strong db => move |remote: RemotesModel| {
        let remote_name = remote.name;

        // The stack containing the window of sync status', as well as extra information for each sync pair.
        let sections = Stack::builder()
            .transition_type(StackTransitionType::OverLeft)
            .transition_duration(500)
            .build();

        // The sections of this stack's window.
        let page = Box::builder()
            .orientation(Orientation::Vertical)
            .vexpand_set(true)
            .vexpand(true)
            .css_classes(vec!["background".to_string()])
            .build();

        // The list of directories to sync.
        let sync_dirs = ListBox::builder()
            .selection_mode(SelectionMode::None)
            .css_classes(vec!["boxed-list".to_string()])
            .build();

        // Add a directory to the stack.
        let add_dir = glib::clone!(@weak window, @weak sections, @weak page, @weak sync_dirs, @strong remote_name, @strong directory_map, @strong sync_dir_deletion_queue => move |
            server_name: String,
            local_path: String,
            remote_path: String,
        | {
            let server_name_owned = server_name.to_string();
            let formatted_local_path = util::fmt_home(&local_path);
            let formatted_remote_path = format!("/{remote_path}");

            // The sync status row.
            let sync_status_sections = Box::builder().orientation(Orientation::Vertical).hexpand(true).hexpand_set(true).margin_start(10).margin_end(10).build();
            let row_sections = Box::builder().orientation(Orientation::Horizontal).hexpand(true).hexpand_set(true).build();
            let status_container = Bin::builder().width_request(30).build();
            status_container.set_child(Some(&get_image("content-loading-symbolic")));
            row_sections.append(&status_container);

            let text_sections = Box::builder().orientation(Orientation::Vertical).valign(Align::Center).hexpand(true).hexpand_set(true).margin_start(10).margin_end(10).margin_top(5).margin_bottom(5).build();
            let title = {
                let sections = Box::builder().orientation(Orientation::Horizontal).build();
                let local_label = Label::builder().label(&formatted_local_path).ellipsize(EllipsizeMode::Start).build();
                let remote_label = Label::builder().label(&formatted_remote_path).ellipsize(EllipsizeMode::Start).build();
                let arrow = get_ui_icon("go-next-symbolic", 16);
                sections.append(&local_label);
                sections.append(&arrow);
                sections.append(&remote_label);
                sections
            };
            let text_status_container = Box::builder().orientation(Orientation::Horizontal).build();
            let error_status = Label::builder()
                .halign(Align::Start)
                .css_classes(vec!["caption".to_string(), "dim-label".to_string(), "error".to_string()])
                .build();
            let status = Label::builder()
                .label(&tr::tr!("Awaiting sync check..."))
                .halign(Align::Start)
                .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
                .ellipsize(EllipsizeMode::End)
                .build();
            text_status_container.append(&error_status);
            text_status_container.append(&status);
            text_sections.append(&title);
            text_sections.append(&text_status_container);
            let progress_bar = ProgressBar::builder()
                .show_text(true)
                .fraction(0.0)
                .hexpand(true)
                .hexpand_set(true)
                .halign(Align::Fill)
                .width_request(260)
                .margin_top(3)
                .build();
            let stats_text = Label::builder()
                .label(&tr::tr!("No sync statistics yet."))
                .halign(Align::Start)
                .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
                .ellipsize(EllipsizeMode::End)
                .build();
            text_sections.append(&progress_bar);
            text_sections.append(&stats_text);

            row_sections.append(&text_sections);

            let more_info_button = get_ui_icon("go-next-symbolic", 18);
            more_info_button.set_halign(Align::End);
            more_info_button.set_hexpand_set(true);
            more_info_button.set_hexpand(true);

            row_sections.append(&more_info_button);
            sync_status_sections.append(&row_sections);

            // The more info page.
            let more_info_page = Box::builder()
                .orientation(Orientation::Vertical)
                .vexpand_set(true)
                .vexpand(true)
                .css_classes(vec!["background".to_string()])
                .build();
            let more_info_header_buttons = Box::builder()
                .orientation(Orientation::Horizontal)
                .margin_bottom(10)
                .build();

            // The errors section.
            let more_info_errors_label = Label::builder()
            .label(&tr::tr!("Sync Errors"))
            .halign(Align::Start)
            .hexpand_set(true)
            .hexpand(true)
            .valign(Align::End)
            .visible(false)
            .margin_bottom(10)
            .css_classes(vec!["heading".to_string()])
            .build();
            let more_info_errors_list = ListBox::builder().selection_mode(SelectionMode::None).css_classes(vec!["boxed-list".to_string()]).margin_top(5).margin_end(5).margin_bottom(5).margin_start(5).build();
            let more_info_errors_list_scrolled = ScrolledWindow::builder().child(&more_info_errors_list).valign(Align::Start).visible(false).build();

            // The statistics section.
            let more_info_stats_label = Label::builder()
                .label(&tr::tr!("Sync Statistics"))
                .halign(Align::Start)
                .hexpand_set(true)
                .hexpand(true)
                .valign(Align::End)
                .margin_bottom(10)
                .css_classes(vec!["heading".to_string()])
                .build();
            let more_info_progress_bar = ProgressBar::builder()
                .show_text(true)
                .fraction(0.0)
                .hexpand(true)
                .hexpand_set(true)
                .halign(Align::Fill)
                .width_request(260)
                .margin_top(5)
                .margin_end(5)
                .margin_bottom(5)
                .margin_start(5)
                .build();
            let more_info_stats_text = Label::builder()
                .label(&tr::tr!("No sync statistics yet."))
                .halign(Align::Start)
                .wrap(true)
                .xalign(0.0)
                .margin_top(5)
                .margin_end(5)
                .margin_bottom(5)
                .margin_start(5)
                .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
                .build();
            let more_info_current_item_text = Label::builder()
                .label(&tr::tr!("Current item: idle"))
                .halign(Align::Start)
                .ellipsize(EllipsizeMode::End)
                .margin_top(5)
                .margin_end(5)
                .margin_bottom(5)
                .margin_start(5)
                .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
                .build();

            // The exclusion list.
            let more_info_exclusions_header = Box::builder().orientation(Orientation::Horizontal).margin_top(20).margin_bottom(10).build();
            let more_info_exclusions_label = Label::builder()
                .label(&tr::tr!("File/Folder Exclusions"))
                .halign(Align::Start)
                .hexpand_set(true)
                .hexpand(true)
                .valign(Align::End)
                .css_classes(vec!["heading".to_string()])
                .build();
            let more_info_exclusions_add_button = Button::builder()
                .child(&get_ui_icon("list-add-symbolic", 16))
                .halign(Align::End)
                .build();
            more_info_exclusions_header.append(&more_info_exclusions_label);
            more_info_exclusions_header.append(&more_info_exclusions_add_button);
            let more_info_exclusions_list = ListBox::builder().selection_mode(SelectionMode::None).css_classes(vec!["boxed-list".to_string()]).valign(Align::Start).margin_top(5).margin_end(5).margin_bottom(5).margin_start(5).build();
            let more_info_exclusions_list_scrolled = ScrolledWindow::builder().child(&more_info_exclusions_list).vexpand_set(true).vexpand(true).build();

            // Read the ignore file to see if anything exists in it so far.
            let file_ignore_path_string = format!("{local_path}/{FILE_IGNORE_NAME}");
            let get_lock = glib::clone!(@strong file_ignore_path_string => move || {
                // This will return an [`Err`] if the parent folder doesn't exist, so handle that case instead of `.unwrap`ing it.
                FileLock::lock(&file_ignore_path_string, true, FileOptions::new().create(true).read(true).write(true).append(false))
            });

            let file_ignore_content = if get_lock().is_ok() {
                Some(fs::read_to_string(&file_ignore_path_string).unwrap())
            } else {
                None
            };

            let ignore_rules: Rc<RefCell<IndexMap<EntryRow, String>>> = Rc::new(RefCell::new(IndexMap::new()));
            let write_file = glib::clone!(@strong file_ignore_path_string, @strong ignore_rules, @strong get_lock => move || {
                let ptr = ignore_rules.get_ref();
                let strings: Vec<String> = ptr.values().map(|item| item.to_owned()).collect();

                // First truncate the file.
                OpenOptions::new().write(true).truncate(true).open(&file_ignore_path_string).unwrap();

                // And then write to it.
                if let Ok(mut lock) = get_lock() {
                    lock.file.write_all(strings.join("\n").as_bytes()).unwrap()
                };
            });
            let gen_ignore_row = glib::clone!(@strong get_lock, @strong write_file, @strong ignore_rules, @strong more_info_exclusions_list => move |content: Option<String>| {
                let row = EntryRow::builder().css_classes(vec!["celeste-no-title".to_string()]).build();
                if let Some(text) = content {
                    row.set_text(&text);
                } else {
                    row.set_show_apply_button(true);
                }
                let remove_button = Button::builder()
                    .child(&get_ui_icon("list-remove-symbolic", 16))
                    .valign(Align::Center)
                    .css_classes(vec!["flat".to_string()])
                    .build();
                row.connect_apply(glib::clone!(@strong get_lock, @strong write_file, @strong ignore_rules => move |row| {
                    // Make sure our ignore rules has the latest string for this item.
                    let mut ptr = ignore_rules.get_mut_ref();
                    ptr.insert(row.clone(), row.text().to_string());
                    drop(ptr);

                    // Write out all the current ignore rules to the file.
                    write_file();
                }));
                remove_button.connect_clicked(glib::clone!(@strong get_lock, @strong write_file, @strong ignore_rules, @weak row, @weak more_info_exclusions_list => move |_| {
                    row.set_sensitive(false);
                    more_info_exclusions_list.remove(&row);

                    // This returns [`None`] if the item hasn't been added via `row.connect_apply` above yet.
                    let mut ptr = ignore_rules.get_mut_ref();
                    if ptr.remove(&row).is_none() {
                        return;
                    }

                    drop(ptr);
                    write_file();
                }));
                row.connect_changed(|row| {
                    let text = row.text().to_string();

                    // If this row is valid, show the apply button. Otherwise, hide it.
                    if let Err(err) = glob::Pattern::new(&text) {
                        row.set_show_apply_button(false);
                        row.add_css_class("error");
                        row.set_tooltip_text(Some(&err.to_string()));
                    } else {
                        row.remove_css_class("error");
                        row.set_tooltip_text(None);
                        row.set_show_apply_button(true);
                    }
                });
                row.add_suffix(&remove_button);
                row
            });
            more_info_exclusions_add_button.connect_clicked(glib::clone!(@weak more_info_exclusions_list, @strong gen_ignore_row => move |_| {
                more_info_exclusions_list.append(&gen_ignore_row(None));
            }));

            if let Some(ignore_content) = file_ignore_content {
                for line in ignore_content.lines() {
                    let line_owned = line.to_owned();
                    let row = gen_ignore_row(Some(line_owned.clone()));
                    more_info_exclusions_list.append(&row);
                    ignore_rules.get_mut_ref().insert(row, line_owned);
                }
            }

            // The back button to go back to the main page.
            let more_info_back_button = Button::builder()
                .child(&get_ui_icon("go-previous-symbolic", 16))
                .halign(Align::Start)
                .hexpand_set(true)
                .hexpand(true)
                .build();
            more_info_back_button.connect_clicked(glib::clone!(@weak sections => move |_| {
                // Temporarily reverse the transition direction so it looks like we're going back a page.
                let previous_transition_type = sections.transition_type();
                sections.set_transition_type(StackTransitionType::OverRight);
                sections.set_visible_child_name("main");
                sections.set_transition_type(previous_transition_type);
            }));
            let more_info_delete_button = Button::builder()
                .child(&get_ui_icon("user-trash-symbolic", 16))
                .has_tooltip(true)
                .tooltip_text(&tr::tr!("Stop syncing this directory"))
                .halign(Align::End)
                .build();

            // Store the pages element's in a vector. When the delete button is pressed and we confirm a deletion, we want the entire page to not be sensitive except for the back button, and we do that by only making the back button sensitive.
            let more_info_widgets: Vec<Widget> = vec![
                more_info_stats_label.clone().into(),
                more_info_progress_bar.clone().into(),
                more_info_stats_text.clone().into(),
                more_info_current_item_text.clone().into(),
                more_info_errors_label.clone().into(),
                more_info_errors_list_scrolled.clone().into(),
                more_info_exclusions_header.clone().into(),
                more_info_exclusions_list_scrolled.clone().into(),
                more_info_back_button.clone().into(),
                more_info_delete_button.clone().into(),
            ];
            more_info_delete_button.connect_clicked(glib::clone!(@strong sync_dir_deletion_queue, @strong server_name, @strong local_path, @strong remote_path, @strong formatted_local_path, @strong formatted_remote_path, @weak sections, @weak more_info_back_button, @weak more_info_delete_button, @strong more_info_widgets => move |_| {
                more_info_widgets.iter().for_each(|item| item.set_sensitive(false));
                let dialog = MessageDialog::builder()
                    .text(
                        &tr::tr!("Are you sure you want to stop syncing '{}' to '{}'?", formatted_local_path, formatted_remote_path)
                    )
                    .buttons(ButtonsType::YesNo)
                    .build();
                dialog.connect_response(glib::clone!(@strong sync_dir_deletion_queue, @strong server_name, @strong local_path, @strong remote_path, @weak sections, @weak more_info_back_button, @weak more_info_delete_button, @strong more_info_widgets => move |dialog, resp| {
                    match resp {
                        ResponseType::Yes => {
                            let data = (server_name.clone(), local_path.clone(), remote_path.clone());
                            sync_dir_deletion_queue.get_mut_ref().push(data);
                            more_info_delete_button.set_tooltip_text(Some(&tr::tr!("This directory is currently being processed to no longer be synced.")));
                            more_info_back_button.set_sensitive(true);
                            dialog.close();
                        },
                        ResponseType::No => {
                            dialog.close();
                            more_info_widgets.iter().for_each(|item| item.set_sensitive(true));
                        },
                        _ => ()
                    }

                }));
                dialog.show();
            }));
            more_info_header_buttons.append(&more_info_back_button);
            more_info_header_buttons.append(&more_info_delete_button);
            more_info_page.append(&more_info_header_buttons);
            more_info_page.append(&more_info_stats_label);
            more_info_page.append(&more_info_progress_bar);
            more_info_page.append(&more_info_stats_text);
            more_info_page.append(&more_info_current_item_text);
            more_info_page.append(&more_info_errors_label);
            more_info_page.append(&more_info_errors_list_scrolled);
            more_info_page.append(&more_info_exclusions_header);
            more_info_page.append(&more_info_exclusions_list_scrolled);

            // Show the window upon click.
            let stack_child_name = format!("{local_path}/{remote_path}");
            let gesture = GestureClick::new();
            let update_error_list = glib::clone!(@weak error_status, @weak more_info_errors_list_scrolled => move || {
                // Ensure the errors section is set up correctly.
                let num_errors = error_status.text().as_str().split_whitespace().next().unwrap_or("0").parse::<i32>().unwrap();

                // Hide the section if we have no errors.
                if num_errors == 0 {
                    error_status.set_visible(false);
                    more_info_errors_list_scrolled.set_visible(false);
                } else if num_errors <= 3 {
                    error_status.set_visible(true);
                    more_info_errors_list_scrolled.set_visible(true);
                    more_info_errors_list_scrolled.set_vscrollbar_policy(PolicyType::Never);
                    more_info_errors_list_scrolled.set_min_content_height(-1);
                } else {
                    error_status.set_visible(true);
                    more_info_errors_list_scrolled.set_visible(true);
                    more_info_errors_list_scrolled.set_vscrollbar_policy(PolicyType::Always);
                    more_info_errors_list_scrolled.set_min_content_height(150 /* 50 px * 3 entries - seems to be the height of a ListBoxRow in Libadwaita */);
                }
            });

            gesture.connect_released(glib::clone!(@weak sections, @strong stack_child_name, @strong update_error_list  => move |_, _, _, _| {
                update_error_list();
                sections.set_visible_child_name(&stack_child_name);
            }));
            sync_status_sections.add_controller(&gesture);

            // Add the items to the directory map.
            let sync_status_sections_container = ListBoxRow::builder().child(&sync_status_sections).build();
            let mut dmap = directory_map.borrow_mut();

            if !dmap.contains_key(&server_name_owned) {
                dmap.insert(server_name_owned, IndexMap::new());
            }

            dmap.get_mut(&server_name).unwrap().insert(
                (local_path, remote_path),
                SyncDir {
                    parent_list: sync_dirs.clone(),
                    container: sync_status_sections_container.clone(),
                    status_icon: status_container,
                    error_status_text: error_status,
                    status_text: status,
                    progress_bar,
                    stats_text,
                    more_info_progress_bar,
                    more_info_stats_text,
                    more_info_current_item_text,
                    error_list: more_info_errors_list,
                    error_items: HashMap::new(),
                    update_error_ui: boxed::Box::new(update_error_list)
                }
            );

            sync_dirs.append(&sync_status_sections_container);
            sections.add_named(&more_info_page, Some(&stack_child_name));
        });

        // Create the remote in the database if it doesn't current exist.
        let db_remote = util::await_future(
                RemotesEntity::find()
                    .filter(RemotesColumn::Name.eq(remote_name.clone()))
                    .one(&db),
            )
            .unwrap().unwrap();

        // The directory header, directory addition button, and remote deletion button.
        {
            let section = Box::builder().orientation(Orientation::Horizontal).build();
            let logo = get_sync_logo();
            logo.set_margin_end(10);
            let label = Label::builder()
                .label(&tr::tr!("Directories"))
                .halign(Align::Start)
                .hexpand(true)
                .hexpand_set(true)
                .valign(Align::End)
                .margin_end(10)
                .css_classes(vec!["heading".to_string()])
                .build();
            let new_folder_button = Button::builder()
                .child(&get_ui_icon("folder-new", 16))
                .halign(Align::End)
                .valign(Align::Start)
                .build();
            new_folder_button.connect_clicked(glib::clone!(@weak window, @weak sections, @weak page, @strong remote_name, @strong sync_dirs, @strong db, @strong directory_map, @strong db_remote, @strong add_dir => @default-panic, move |_| {
                window.set_sensitive(false);
                let folder_window = ApplicationWindow::builder()
                    .title(&util::get_title!("Remote Folder Picker"))
                    .build();
                folder_window.add_css_class("celeste-global-padding");
                let folder_sections = Box::builder().orientation(Orientation::Vertical).build();
                folder_sections.append(&HeaderBar::new());

                // Get the local folder to sync with.
                let local_label = Label::builder().label(&tr::tr!("Local folder:")).halign(Align::Start).css_classes(vec!["heading".to_string()]).build();
                let local_entry = Entry::builder()
                    .secondary_icon_activatable(true)
                    .secondary_icon_name("folder-symbolic")
                    .secondary_icon_sensitive(true)
                    .build();
                local_entry.connect_icon_press(glib::clone!(@weak folder_window, @weak local_label => move |local_entry, _| {
                    folder_window.set_sensitive(false);
                    let filter = FileFilter::new();
                    filter.add_mime_type("inode/directory");
                    let dialog = FileChooserDialog::builder()
                        .title(&util::get_title!("Local Folder Picker"))
                        .select_multiple(false)
                        .create_folders(true)
                        .filter(&filter)
                        .build();
                    let cancel_button = Button::with_label(&tr::tr!("Cancel"));
                    let ok_button = Button::with_label(&tr::tr!("Ok"));
                    dialog.add_action_widget(&cancel_button, ResponseType::Cancel);
                    dialog.add_action_widget(&ok_button, ResponseType::Ok);
                    dialog.connect_close_request(glib::clone!(@strong folder_window => move |_| {
                        folder_window.set_sensitive(true);
                        Inhibit(false)
                    }));
                    cancel_button.connect_clicked(glib::clone!(@weak folder_window, @weak dialog => move |_| {
                        dialog.close();
                    }));
                    ok_button.connect_clicked(glib::clone!(@weak folder_window, @weak local_entry, @weak dialog => move |_| {
                        local_entry.set_text(&dialog.file().unwrap().path().unwrap().into_os_string().into_string().unwrap());
                        dialog.close();
                    }));
                    dialog.show();
                }));

                // Get the remote folder to sync with, and add it.
                // The entry completion code is largely inspired by https://github.com/gtk-rs/gtk4-rs/blob/master/examples/entry_completion/main.rs. I honestly have no clue what half the code for that is doing, I just know the current code is working well enough, and it can be fixed later if it breaks.
                let remote_label = Label::builder().label(&tr::tr!("Remote folder:")).halign(Align::Start).css_classes(vec!["heading".to_string()]).build();
                let entry_completion = EntryCompletion::new();
                let store = ListStore::new(&[glib::Type::STRING]);

                // The path that this store is currently valid on, excluding everything after the
                // last `/` in the UI. We use this to detect when we need to obtain the list of
                // directories from the remote again. The [`Vec`] of [`String`]s is a vector of
                // rightmost dir items (i.e. it would contain `bar` instead of `/foo/bar`) because
                // of how `update_options` is called below, so checks need to be done to make sure
                // that the currently typed in path is the same as the one in the tuple's [`Path`]
                // element.
                let store_path: Rc<RefCell<(PathBuf, Vec<String>)>> = Rc::new(RefCell::new((Path::new("").to_owned(), vec![])));

                entry_completion.set_text_column(0);
                entry_completion.set_popup_completion(true);
                entry_completion.set_model(Some(&store));
                let remote_entry = Entry::builder().completion(&entry_completion).build();
                remote_entry.insert_text("/", &mut -1);

                // Get the current path, up to the last '/'.
                let get_current_path = glib::clone!(@weak remote_entry => @default-panic, move || {
                    let text = remote_entry.text().to_string();
                    if text.ends_with('/') {
                        Path::new(&text).to_path_buf()
                    } else {
                        Path::new(&text).parent().unwrap_or_else(|| Path::new("")).to_path_buf()
                    }
                });

                // Update the UI completions against the list of stored directories.
                let update_completions = glib::clone!(@weak entry_completion, @strong store, @weak remote_entry, @weak store, @strong store_path, @strong get_current_path => move || {

                    // Get the current specified directory.
                    let current_item_text = remote_entry.text();
                    let current_item = Path::new(current_item_text.as_str()).file_name().map(|path| path.to_str().unwrap()).unwrap_or("");

                    // Clear the current list of completions.
                    store.clear();

                    // See if any of the currently stored matches start with the same characters as
                    // our path, and if they do, append them to the valid completions list.
                    for item in &store_path.get_ref().1 {
                        if item.starts_with(current_item) {
                            store.set(&store.append(), &[(0, item)]);
                        }
                    }
                });

                // The entry completion logic.
                entry_completion.set_match_func(glib::clone!(@weak remote_entry => @default-panic, move |entry_completion, _entry_str, tree_iter| {
                    let tree_model = entry_completion.model().unwrap();
                    let text_column = entry_completion.text_column();
                    let text_value = match tree_model.get_value(tree_iter, text_column).get::<String>() {
                        // Not quite sure when this could fail, but it does sometimes, so return early when that's the case.
                        Ok(value) => value,
                        Err(_) => return false
                    };

                    // The last component of the directory specified by the user.
                    let remote_entry_text = remote_entry.text().to_string();
                    let entry_final_path_item = Path::new(&remote_entry_text).file_name().map(|path| path.to_str().unwrap()).unwrap_or("");
                    text_value.starts_with(entry_final_path_item)
                }));

                entry_completion.connect_match_selected(glib::clone!(@weak remote_entry => @default-panic, move |_, model, iter| {
                    let selected_entry = model.get::<String>(iter, 0);
                    // The current text up to the last slash (i.e. 'hi' in '/foo/bar/hi').
                    let up_to_slash_text = 'slash: {
                        let current_text = remote_entry.text().to_string();

                        // If the current text doesn't contain a slash, just return all the currently entered text.
                        if !current_text.contains('/') {
                            break 'slash current_text
                        }

                        // Otherwise return the text up to the last slash.
                        break 'slash match current_text.rsplit_once('/') {
                            Some((_, string)) => string.to_string(),
                            None => String::new()
                        }
                    };

                    // Get the text that we need to append.
                    let mut to_append = selected_entry.strip_prefix(&up_to_slash_text).unwrap().to_string();
                    to_append.push('/');

                    // Append the text, and set the position to the end of the entry box.
                    remote_entry.insert_text(&to_append, &mut -1);
                    remote_entry.set_position(-1);

                    // Stop the default matching behavior since we handled it here.
                    Inhibit(true)
                }));

                // Update the stored list of autocompletions to the parent of those of the currently typed in directory.
                let update_options = glib::clone!(@strong remote_name, @strong store_path, @weak remote_entry, @strong update_completions, @strong get_current_path => move || {
                    let current_path = get_current_path();
                    let current_path_string = current_path.as_os_str().to_owned().into_string().unwrap();

                    let items = if let Ok(items) = rclone::sync::list(&remote_name, &current_path_string, false, RcloneListFilter::Dirs) {
                        items.into_iter().map(|item| item.name).collect()
                    } else {
                        vec![]
                    };

                    // If the current parent path is still the same (i.e. after the file listing above has finished, which may have taken a bit), then update the completions to reflect the items we got.
                    let mut store_path_ref = store_path.get_mut_ref();

                    if store_path_ref.0 == current_path {
                        store_path_ref.1 = items;
                        // Drop `store_path_ref` so `update_completions` can get its own reference.
                        drop(store_path_ref);
                        update_completions();
                    }
                });

                remote_entry.connect_cursor_position_notify(glib::clone!(@strong remote_name, @weak store_path, @strong update_completions, @strong update_options, @strong get_current_path => move |_| {
                    // For some reason we have to clone the closure to pass the borrow checker, even though we clone it via the 'glib::clone!' above. Not sure why yet.
                    let update_options = update_options.clone();

                    let current_path = get_current_path();

                    let mut store_path_ref = store_path.get_mut_ref();

                    if store_path_ref.0 == current_path {
                        // Drop our ref to `store_path_ref` so `update_completions` can get it's own.
                        drop(store_path_ref);
                        update_completions();
                    } else {
                        store_path_ref.0 = current_path;
                        // Drop our ref to `store_path_ref` so `update_options` can get it's own.
                        drop(store_path_ref);
                        update_options();
                    }
                }));

                folder_sections.append(&local_label);
                folder_sections.append(&local_entry);
                folder_sections.append(&Separator::builder().orientation(Orientation::Vertical).css_classes(vec!["spacer".to_string()]).build());
                folder_sections.append(&remote_label);
                folder_sections.append(&remote_entry);
                let confirm_box = Box::builder().orientation(Orientation::Horizontal).spacing(10).halign(Align::End).build();
                let cancel_button = Button::with_label(&tr::tr!("Cancel"));
                let ok_button = Button::with_label(&tr::tr!("Ok"));
                confirm_box.append(&cancel_button);
                confirm_box.append(&ok_button);
                folder_sections.append(&Separator::builder().orientation(Orientation::Vertical).css_classes(vec!["spacer".to_string()]).build());
                folder_sections.append(&confirm_box);

                // If either entry is empty, don't allow the button to be clicked.
                // Also initialize the button as non-clickable.
                ok_button.set_sensitive(false);

                local_entry.connect_changed(glib::clone!(@weak ok_button, @weak remote_entry => move |local_entry| {
                    if local_entry.to_string().is_empty() || remote_entry.to_string().is_empty() {
                        ok_button.set_sensitive(false);
                    } else {
                        ok_button.set_sensitive(true);
                    }
                }));
                remote_entry.connect_changed(glib::clone!(@weak ok_button, @weak local_entry => move |remote_entry| {
                    if local_entry.to_string().is_empty() || remote_entry.to_string().is_empty() {
                        ok_button.set_sensitive(false);
                    } else {
                        ok_button.set_sensitive(true);
                    }
                }));

                folder_window.connect_close_request(glib::clone!(@strong window => move |_| {
                    window.set_sensitive(true);
                    Inhibit(false)
                }));
                cancel_button.connect_clicked(glib::clone!(@strong window, @weak folder_window => move |_| {
                    folder_window.close();
                    window.set_sensitive(true);
                }));
                ok_button.connect_clicked(glib::clone!(@strong window, @weak sections, @weak folder_window, @weak sync_dirs, @weak local_entry, @weak remote_entry, @strong db_remote, @strong db, @weak directory_map, @strong remote_name, @strong add_dir => move |_| {
                    folder_window.set_sensitive(false);

                    // The local path needs to start with a slash, but not end with one. The remote
                    // needs to not start or end with a slash.
                    let local_text = "/".to_string() + &util::strip_slashes(local_entry.text().as_str());
                    let remote_text = util::strip_slashes(remote_entry.text().as_str());
                    let local_path = Path::new(&local_text);
                    match rclone::sync::stat(&remote_name, &remote_text) {
                        Ok(path) => {
                            if path.is_none() {
                                gtk_util::show_error(&tr::tr!("The specified remote directory doesn't exist"), None);
                                folder_window.set_sensitive(true);
                                return;
                            } else {
                                path
                            }
                        },
                        Err(err) => {
                            gtk_util::show_error(&tr::tr!("Failed to check if the specified remote directory exists"), Some(&err.error));
                            folder_window.set_sensitive(true);
                            return;
                        }
                    };

                    let sync_dir = util::await_future(
                        SyncDirsEntity::find().filter(SyncDirsColumn::LocalPath.eq(local_text.clone())).filter(SyncDirsColumn::RemotePath.eq(remote_text.clone())).one(&db)
                    ).unwrap();

                    if sync_dir.is_some() {
                        gtk_util::show_error(&tr::tr!("The specified directory pair is already being synced"), None);
                        folder_window.set_sensitive(true);
                    } else if !local_path.exists() {
                        gtk_util::show_error(&tr::tr!("The specified local directory doesn't exist"), None);
                        folder_window.set_sensitive(true);
                    } else if !local_path.is_dir() {
                        gtk_util::show_error(&tr::tr!("The specified local path isn't a directory"), None);
                        folder_window.set_sensitive(true);
                    } else if !local_path.is_absolute() {
                        gtk_util::show_error(&tr::tr!("The specified local directory needs to be an absolute path"), None);
                        folder_window.set_sensitive(true);
                    } else {
                        util::await_future(
                            SyncDirsActiveModel {
                                remote_id: ActiveValue::Set(db_remote.id),
                                local_path: ActiveValue::Set(local_text.clone()),
                                remote_path: ActiveValue::Set(remote_text.clone()),
                                ..Default::default()
                            }.insert(&db)
                        ).unwrap();
                        add_dir(remote_name.clone(), local_text, remote_text);
                        folder_window.close();
                    }
                }));

                folder_window.set_content(Some(&folder_sections));
                folder_window.show();
            }));
            let delete_remote_button = Button::builder()
                .child(&get_ui_icon("user-trash-symbolic", 16))
                .halign(Align::End)
                .valign(Align::Start)
                .margin_start(10)
                .build();
            delete_remote_button.connect_clicked(glib::clone!(@strong remote_deletion_queue, @strong page, @strong remote_name => move |delete_remote_button| {
                page.set_sensitive(false);
                let dialog = MessageDialog::builder()
                    .text(&tr::tr!("Are you sure you want to delete this remote?"))
                    .secondary_text(&tr::tr!("All the directories associated with this remote will also stop syncing."))
                    .buttons(ButtonsType::YesNo)
                    .build();
                dialog.connect_response(glib::clone!(@strong remote_deletion_queue, @strong page, @strong remote_name, @weak delete_remote_button => move |dialog, resp| {
                    match resp {
                        ResponseType::Yes => {
                            remote_deletion_queue.get_mut_ref().push(remote_name.clone());
                            dialog.close();
                        },
                        ResponseType::No => {
                            dialog.close();
                            page.set_sensitive(true);
                        }
                        _ => ()
                    }
                }));
                dialog.show();
            }));
            section.append(&logo);
            section.append(&label);
            section.append(&new_folder_button);
            section.append(&delete_remote_button);
            page.append(&section);
        }

        // The directory listing.
        {
            // Get the currently present directories.
            let dirs = util::await_future(
                SyncDirsEntity::find()
                    .filter(SyncDirsColumn::RemoteId.eq(db_remote.id))
                    .all(&db),
            )
            .unwrap();
            // Create the entry for each directory.
            for dir in dirs {
                add_dir(
                    db_remote.name.clone(),
                    dir.local_path.clone(),
                    dir.remote_path.clone(),
                );
            }
        }
        page.append(&gtk_util::separator());
        page.append(&sync_dirs);

        sections.add_named(&page, Some("main"));
        sections.set_visible_child_name("main");
        sections
    });

    for remote in remotes {
        let window = gen_remote_window(remote.clone());
        stack.add_titled(&window, Some(&remote.name), &remote.name);
        let server_row = build_server_row(&remote.name);
        server_list.append(&server_row.row);
        server_rows
            .borrow_mut()
            .insert(remote.name.clone(), server_row);
    }

    // Set up the main sections.
    let sections = Leaflet::builder()
        .transition_type(LeafletTransitionType::Slide)
        .css_classes(vec!["main".to_string()])
        .build();

    let sidebar_box = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["sidebar".to_string()])
        .build();
    let sidebar_header = HeaderBar::builder().decoration_layout("").build();
    let sidebar_add_server_button = icon_button("list-add-symbolic", 16);
    sidebar_add_server_button.connect_clicked(
        glib::clone!(@weak app, @weak window, @weak stack, @weak server_list, @strong server_rows, @strong gen_remote_window, @strong db => move |_| {
            window.set_sensitive(false);

            if let Some(remotes) = login::login(&app, &db) {
                for remote in remotes {
                    if stack.child_by_name(&remote.name).is_some() {
                        continue;
                    }

                    let window = gen_remote_window(remote.clone());
                    stack.add_titled(&window, Some(&remote.name), &remote.name);
                    let server_row = build_server_row(&remote.name);
                    server_list.append(&server_row.row);
                    server_rows
                        .borrow_mut()
                        .insert(remote.name.clone(), server_row);
                }
            }

            window.set_sensitive(true);
        }),
    );
    let sidebar_menu_button = icon_button("open-menu-symbolic", 16);
    let sidebar_menu_popover_sections = Box::new(Orientation::Vertical, 5);
    let sidebar_menu_popover = Popover::builder()
        .child(&sidebar_menu_popover_sections)
        .position(PositionType::Bottom)
        .build();
    let sidebar_menu_test_connections_button = Button::builder()
        .label("Test Connections")
        .css_classes(vec!["flat".to_string()])
        .build();
    sidebar_menu_test_connections_button.connect_clicked(
        glib::clone!(@weak window, @weak sidebar_menu_popover, @strong server_rows, @strong server_status_checked => move |_| {
            sidebar_menu_popover.popdown();
            window.set_sensitive(false);

            // Collect the remote names up front so we don't hold a borrow on
            // `server_rows` while `set_server_status` (which also borrows it)
            // runs.
            let remote_names: Vec<String> = server_rows.borrow().keys().cloned().collect();

            // Show every row as "checking" first so the user gets immediate
            // feedback that the test has started.
            for remote_name in &remote_names {
                set_server_status(&server_rows, remote_name, ServerStatus::Checking);
            }
            util::process_ui_events();

            for remote_name in &remote_names {
                let started = Instant::now();
                util::log_line(&format!(
                    "manual connection test start remote={}",
                    remote_name
                ));
                let ok = rclone::sync::stat(remote_name, "/").is_ok();
                util::log_line(&format!(
                    "manual connection test finished remote={} ok={} elapsed_ms={}",
                    remote_name,
                    ok,
                    started.elapsed().as_millis()
                ));
                // Record the result so the background sync loop treats this
                // remote as already checked and doesn't re-run its own probe.
                server_status_checked
                    .borrow_mut()
                    .insert(remote_name.clone(), ok);
                set_server_status(
                    &server_rows,
                    remote_name,
                    if ok {
                        ServerStatus::Working
                    } else {
                        ServerStatus::Broken
                    },
                );
                util::process_ui_events();
            }

            window.set_sensitive(true);
        }),
    );
    let sidebar_menu_about_button = Button::builder()
        .label("About")
        .css_classes(vec!["flat".to_string()])
        .build();
    sidebar_menu_about_button.connect_clicked(
        glib::clone!(@weak app, @weak sidebar_menu_popover => move |_| {
            sidebar_menu_popover.popdown();
            crate::about::about_window(&app);
        }),
    );
    let sidebar_menu_quit_button = Button::builder()
        .label("Quit")
        .css_classes(vec!["flat".to_string()])
        .build();
    sidebar_menu_quit_button.connect_clicked(glib::clone!(@weak sidebar_menu_popover => move |_| {
        sidebar_menu_popover.popdown();
        *(*CLOSE_REQUEST).lock().unwrap() = true;
    }));
    sidebar_menu_popover_sections.append(&sidebar_menu_test_connections_button);
    sidebar_menu_popover_sections.append(&sidebar_menu_about_button);
    sidebar_menu_popover_sections.append(&sidebar_menu_quit_button);
    sidebar_menu_popover.set_parent(&sidebar_menu_button);
    sidebar_menu_button.connect_clicked(glib::clone!(@weak sidebar_menu_popover => move |_| {
        sidebar_menu_popover.popup();
    }));
    let sidebar_nav_right_button = icon_button("go-next-symbolic", 16);
    sidebar_header.pack_start(&sidebar_add_server_button);
    sidebar_header.pack_end(&sidebar_menu_button);
    sidebar_box.append(&sidebar_header);
    sidebar_box.append(&server_list_scrolled);

    let stack_box = Box::builder()
        .orientation(Orientation::Vertical)
        .hexpand_set(true)
        .hexpand(true)
        .css_classes(vec!["stack".to_string()])
        .build();
    let stack_window_title = WindowTitle::new(
        &util::get_title!("{}", stack.visible_child_name().unwrap()),
        "",
    );
    stack.connect_visible_child_notify(glib::clone!(@weak sections, @weak stack_box, @weak stack_window_title, @weak server_list, @strong server_rows => move |stack| {
        let visible_child_name = stack.visible_child_name().unwrap();
        stack_window_title.set_title(&util::get_title!("{}", visible_child_name));
        if let Some(server_row) = server_rows.borrow().get(visible_child_name.as_str()) {
            server_list.select_row(Some(&server_row.row));
        }
        sections.set_visible_child(&stack_box);
    }));
    let stack_header = HeaderBar::builder()
        .title_widget(&stack_window_title)
        .build();
    let stack_nav_left_button = icon_button("go-previous-symbolic", 16);
    stack_box.append(&stack_header);
    stack_box.append(&stack);

    sections.append(&sidebar_box);
    sections.append(&stack_box);
    sections.set_visible_child(&stack_box);

    sidebar_nav_right_button.connect_clicked(
        glib::clone!(@weak sections, @weak stack_box => move |_| {
            sections.set_visible_child(&stack_box);
        }),
    );
    server_list.connect_row_selected(
        glib::clone!(@weak stack, @weak sections, @weak stack_box => move |_, row| {
            if let Some(row) = row
                && let Some(remote_name) = server_name_from_row(row)
            {
                stack.set_visible_child_name(&remote_name);
                sections.set_visible_child(&stack_box);
            }
        }),
    );
    if let Some(visible_child_name) = stack.visible_child_name()
        && let Some(server_row) = server_rows.borrow().get(visible_child_name.as_str())
    {
        server_list.select_row(Some(&server_row.row));
    }
    stack_nav_left_button.connect_clicked(
        glib::clone!(@weak sections, @weak sidebar_box => move |_| {
            sections.set_visible_child(&sidebar_box);
        }),
    );

    // This is to be used in `connect_folded_notify` below, but we extract it into a
    // separate closure so we can call it once before the UI is shown.
    let folded_notify = glib::clone!(@weak sections, @weak sidebar_header, @weak stack_header, @weak sidebar_nav_right_button, @weak sidebar_menu_button, @weak stack_nav_left_button => move || {
        if sections.is_folded() {
            if sidebar_menu_button.parent().as_ref() == Some(&sidebar_header.clone().upcast::<Widget>()) {
                sidebar_header.remove(&sidebar_menu_button);
            }
            if sidebar_nav_right_button.parent().is_none() {
                sidebar_header.pack_end(&sidebar_nav_right_button);
            }
            if sidebar_menu_button.parent().is_none() {
                sidebar_header.pack_end(&sidebar_menu_button);
            }
            if stack_nav_left_button.parent().is_none() {
                stack_header.pack_start(&stack_nav_left_button);
            }
        } else {
            if sidebar_nav_right_button.parent().as_ref() == Some(&sidebar_header.clone().upcast::<Widget>()) {
                sidebar_header.remove(&sidebar_nav_right_button);
            }
            if stack_nav_left_button.parent().as_ref() == Some(&stack_header.clone().upcast::<Widget>()) {
                stack_header.remove(&stack_nav_left_button);
            }
        }
    });
    sections.connect_folded_notify(glib::clone!(@strong folded_notify => move |_| {
        folded_notify();
    }));
    folded_notify();

    sections.set_visible_child(&sidebar_box);
    window.set_content(Some(&sections));

    // We have to manually close the window when the close button is clicked for some reason. See https://matrix.to/#/!CxdTjqASmMdXwTeLsR:matrix.org/$16724077630uSZSF:hunterwittenborn.com?via=gnome.org&via=matrix.org&via=tchncs.de.
    window.connect_close_request(|window| {
        window.hide();
        Inhibit(true)
    });

    // Show the window, start up the tray, and start syncing.
    if !background {
        window.show();
    }

    let service = ksni::TrayService::new(crate::tray::Tray::new());
    let handle = service.handle();
    service.spawn();

    let sync_errors_count = glib::clone!(@strong directory_map => move || {
        let dmap = directory_map.get_ref();
        let mut error_count = 0;

        for remote_dirs in dmap.values() {
            for dir in remote_dirs.values() {
                if !dir.error_status_text.text().is_empty() {
                    error_count += 1;
                }
            }
        }

        error_count
    });

    'main: loop {
        // Break the loop if the user requested to quit the application.
        if *(*CLOSE_REQUEST).lock().unwrap() {
            break 'main;
        }

        // If the user requested to open the application, then open it up.
        let check_open_requests = glib::clone!(@weak window => move || {
            if *(*OPEN_REQUEST).lock().unwrap() {
                window.show();
                *(*OPEN_REQUEST).lock().unwrap() = false;
            }
        });

        // Continue with syncing.
        let remotes = util::await_future(RemotesEntity::find().all(&db)).unwrap();

        // If no remotes are present we need to close the window and ask the user to log
        // in again.
        if remotes.is_empty() {
            window.close();

            if let Some(remotes) = login::login(app, &db) {
                for remote in remotes {
                    if stack.child_by_name(&remote.name).is_some() {
                        continue;
                    }

                    let window = gen_remote_window(remote.clone());
                    stack.add_titled(&window, Some(&remote.name), &remote.name);
                    let server_row = build_server_row(&remote.name);
                    server_list.append(&server_row.row);
                    server_rows
                        .borrow_mut()
                        .insert(remote.name.clone(), server_row);
                    window.show();
                }
                continue;
            } else {
                break 'main;
            }
        }

        util::sleep_while_processing_events(Duration::from_millis(500));

        let mut checked_idle_remote_this_cycle = false;

        for remote in remotes {
            // Process any remote deletion requests.
            {
                let mut remote_queue = remote_deletion_queue.get_mut_ref();

                while !remote_queue.is_empty() {
                    let remote_name = remote_queue.remove(0);

                    // Remove the item from the UI.
                    let child = stack.child_by_name(&remote_name).unwrap();
                    stack.remove(&child);
                    if let Some(server_row) = server_rows.borrow_mut().shift_remove(&remote_name) {
                        server_list.remove(&server_row.row);
                    }
                    server_status_checked.borrow_mut().remove(&remote_name);

                    // Delete all related database entries.
                    util::await_future(async {
                        let db_remote = RemotesEntity::find()
                            .filter(RemotesColumn::Name.eq(remote_name.clone()))
                            .one(&db)
                            .await
                            .unwrap()
                            .unwrap();
                        let sync_dirs = SyncDirsEntity::find()
                            .filter(SyncDirsColumn::RemoteId.eq(db_remote.id))
                            .all(&db)
                            .await
                            .unwrap();

                        for sync_dir in sync_dirs {
                            SyncItemsEntity::delete_many()
                                .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                                .exec(&db)
                                .await
                                .unwrap();
                            sync_dir.delete(&db).await.unwrap();
                        }

                        db_remote.delete(&db).await.unwrap();
                    });

                    // Delete the Rclone config.
                    rclone::sync::delete_config(&remote_name).unwrap();
                }
            }

            let sync_dirs = util::await_future(
                SyncDirsEntity::find()
                    .filter(SyncDirsColumn::RemoteId.eq(remote.id))
                    .all(&db),
            )
            .unwrap();

            let mut remote_failed = false;

            if sync_dirs.is_empty() {
                let already_checked = server_status_checked.borrow().contains_key(&remote.name);
                if !already_checked && !checked_idle_remote_this_cycle {
                    checked_idle_remote_this_cycle = true;
                    set_server_status(&server_rows, &remote.name, ServerStatus::Checking);
                    util::process_ui_events();
                    let started = Instant::now();
                    util::log_line(&format!(
                        "server status check start remote={} reason=no-sync-dirs",
                        remote.name
                    ));
                    let ok = rclone::sync::stat(&remote.name, "/").is_ok();
                    util::log_line(&format!(
                        "server status check finished remote={} ok={} elapsed_ms={}",
                        remote.name,
                        ok,
                        started.elapsed().as_millis()
                    ));
                    server_status_checked
                        .borrow_mut()
                        .insert(remote.name.clone(), ok);
                    set_server_status(
                        &server_rows,
                        &remote.name,
                        if ok {
                            ServerStatus::Working
                        } else {
                            ServerStatus::Broken
                        },
                    );
                }
                continue;
            }

            for sync_dir in sync_dirs {
                let stats: SyncStatsRef = Rc::new(RefCell::new(SyncStats::default()));
                let sync_dir_started = Instant::now();
                util::log_line(&format!(
                    "sync dir check start remote={} local={} remote_path={}",
                    remote.name, sync_dir.local_path, sync_dir.remote_path
                ));

                // Get the list of local and remote files for this sync
                // directory, and if they don't match the last sync state
                // in the database, then continue with syncing.
                let mut should_sync = false;

                // Local file checks.
                let local_scan_started = Instant::now();
                let local_glob = format!("{}/**/*", sync_dir.local_path);
                if let Ok(paths) = glob::glob(&local_glob) {
                    for maybe_path in paths {
                        if let Ok(path) = maybe_path {
                            let local_scanned = {
                                let mut stats_ref = stats.borrow_mut();
                                stats_ref.local_scanned += 1;
                                stats_ref.local_scanned
                            };
                            maybe_process_ui_events(local_scanned);
                            update_sync_stats_ui(
                                &directory_map,
                                &remote.name,
                                &sync_dir,
                                &stats.borrow(),
                                path.to_str(),
                            );
                            let file = match File::open(&path) {
                                Ok(file) => file,
                                Err(_) => {
                                    should_sync = true;
                                    break;
                                }
                            };
                            let current_timestamp = file
                                .metadata()
                                .unwrap()
                                .modified()
                                .unwrap()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            let maybe_db_sync_item = util::await_future(
                                SyncItemsEntity::find()
                                    .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                                    .filter(
                                        SyncItemsColumn::LocalPath.eq(path.display().to_string()),
                                    )
                                    .one(&db),
                            )
                            .unwrap();

                            let db_timestamp: u64 = if let Some(db_sync_item) = maybe_db_sync_item {
                                db_sync_item.last_local_timestamp.try_into().unwrap()
                            } else {
                                should_sync = true;
                                break;
                            };

                            if current_timestamp != db_timestamp {
                                should_sync = true;
                                break;
                            }
                        } else {
                            should_sync = true;
                            break;
                        }
                    }
                } else {
                    // TODO: We should show the user an error instead of trying to sync again.
                    should_sync = true;
                }
                util::log_line(&format!(
                    "sync dir local scan finished remote={} local={} scanned={} should_sync={} elapsed_ms={}",
                    remote.name,
                    sync_dir.local_path,
                    stats.borrow().local_scanned,
                    should_sync,
                    local_scan_started.elapsed().as_millis()
                ));

                // Remote file checks.
                let remote_scan_started = Instant::now();
                if let Ok(paths) = rclone::sync::list(
                    &remote.name,
                    &sync_dir.remote_path,
                    true,
                    RcloneListFilter::All,
                ) {
                    for path in paths {
                        let remote_scanned = {
                            let mut stats_ref = stats.borrow_mut();
                            stats_ref.remote_scanned += 1;
                            stats_ref.remote_scanned
                        };
                        maybe_process_ui_events(remote_scanned);
                        update_sync_stats_ui(
                            &directory_map,
                            &remote.name,
                            &sync_dir,
                            &stats.borrow(),
                            Some(&path.path),
                        );
                        let stripped_path = match path.name.contains('/') {
                            true => path
                                .name
                                .strip_suffix(&format!("{}/", sync_dir.remote_path))
                                .unwrap()
                                .to_string(),
                            false => path.name.clone(),
                        };
                        let maybe_db_sync_item = util::await_future(
                            SyncItemsEntity::find()
                                .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                                .filter(SyncItemsColumn::RemotePath.eq(stripped_path))
                                .one(&db),
                        )
                        .unwrap();
                        let db_timestamp: i64 = if let Some(db_sync_item) = maybe_db_sync_item {
                            db_sync_item.last_remote_timestamp.into()
                        } else {
                            should_sync = true;
                            break;
                        };
                        let remote_timestamp = path.mod_time.unix_timestamp();

                        if remote_timestamp != db_timestamp {
                            should_sync = true;
                            break;
                        }
                    }
                } else {
                    // TODO: We should show the disconnected icon instead of trying to sync again.
                    remote_failed = true;
                    should_sync = true;
                }
                util::log_line(&format!(
                    "sync dir remote scan finished remote={} remote_path={} scanned={} should_sync={} elapsed_ms={}",
                    remote.name,
                    sync_dir.remote_path,
                    stats.borrow().remote_scanned,
                    should_sync,
                    remote_scan_started.elapsed().as_millis()
                ));

                // DB file checks. This covers files that got deleted locally or on the remote,
                // as those changes wouldn't neccesarily be visible above.
                let db_check_started = Instant::now();
                let sync_items = util::await_future(
                    SyncItemsEntity::find()
                        .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                        .all(&db),
                )
                .unwrap();
                stats.borrow_mut().known_items = sync_items.len() as u64;
                update_sync_stats_ui(
                    &directory_map,
                    &remote.name,
                    &sync_dir,
                    &stats.borrow(),
                    None,
                );

                let mut db_checked = 0_u64;
                for sync_item in sync_items {
                    db_checked += 1;
                    maybe_process_ui_events(db_checked);
                    let remote_path = if !sync_dir.remote_path.is_empty() {
                        format!("{}/{}", sync_dir.remote_path, sync_item.remote_path)
                    } else {
                        sync_item.remote_path.clone()
                    };
                    let maybe_remote_timestamp: Option<i32> =
                        match rclone::sync::stat(&remote.name, &remote_path) {
                            Ok(remote_item) => remote_item.map(|remote_item| {
                                remote_item.mod_time.unix_timestamp().try_into().unwrap()
                            }),
                            Err(err) => {
                                remote_failed = true;
                                util::log_line(&format!(
                                    "server status check failed remote={} path={} error={}",
                                    remote.name, remote_path, err.error
                                ));
                                None
                            }
                        };

                    // If the path doesn't exist both locally and on the remote, then we need to
                    // delete the DB entry.
                    if !Path::new(&sync_item.local_path).exists()
                        && maybe_remote_timestamp.is_none()
                    {
                        util::await_future(async {
                            SyncItemsEntity::find()
                                .filter(SyncItemsColumn::Id.eq(sync_item.id))
                                .one(&db)
                                .await
                                .unwrap()
                                .unwrap()
                                .delete(&db)
                                .await
                                .unwrap();
                        });
                        continue;
                    }

                    let remote_timestamp = match maybe_remote_timestamp {
                        Some(timestamp) => timestamp,
                        None => {
                            should_sync = true;
                            break;
                        }
                    };

                    let local_timestamp: i32 = match fs::metadata(&sync_item.local_path) {
                        Ok(metadata) => metadata
                            .modified()
                            .unwrap()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            .try_into()
                            .unwrap(),
                        Err(_) => {
                            should_sync = true;
                            break;
                        }
                    };

                    if local_timestamp != sync_item.last_local_timestamp
                        || remote_timestamp != sync_item.last_remote_timestamp
                    {
                        should_sync = true;
                        break;
                    }
                }
                util::log_line(&format!(
                    "sync dir db check finished remote={} local={} known_items={} checked={} should_sync={} elapsed_ms={}",
                    remote.name,
                    sync_dir.local_path,
                    stats.borrow().known_items,
                    db_checked,
                    should_sync,
                    db_check_started.elapsed().as_millis()
                ));

                let item_ptr = directory_map.get_ref();
                let item = item_ptr
                    .get(&remote.name)
                    .unwrap()
                    .get(&(sync_dir.local_path.clone(), sync_dir.remote_path.clone()))
                    .unwrap();

                if !should_sync {
                    item.status_text.set_label(&tr::tr!("Files are synced."));
                    item.status_icon
                        .set_child(Some(&get_image("object-select-symbolic")));
                    update_sync_stats_ui(
                        &directory_map,
                        &remote.name,
                        &sync_dir,
                        &stats.borrow(),
                        None,
                    );
                    util::log_line(&format!(
                        "sync dir check clean remote={} local={} elapsed_ms={}",
                        remote.name,
                        sync_dir.local_path,
                        sync_dir_started.elapsed().as_millis()
                    ));
                    continue;
                }
                util::log_line(&format!(
                    "sync dir sync needed remote={} local={} elapsed_ms={}",
                    remote.name,
                    sync_dir.local_path,
                    sync_dir_started.elapsed().as_millis()
                ));

                // Notify the tray app that we're syncing this remote now. We
                // do this here instead for each remote, because the above
                // sync dir check might make us not change anything on this remote.
                handle.update(|tray| {
                    tray.set_msg(tr::tr!("Syncing '{}'...", remote.name));
                    tray.set_syncing();
                });

                // If we have pending errors that need resolved, don't sync this directory.
                if item.error_status_text.text().len() != 0 {
                    continue;
                }

                // Set up the UI for notifying the user that this directory is being synced.
                // The width/height and margins for this are based on those from `get_image()`
                // at the top of this file, as they're placed at the same place in the UI.
                let spinner = Spinner::builder()
                    .spinning(true)
                    .width_request(4)
                    .height_request(4)
                    .margin_start(3)
                    .margin_end(3)
                    .build();
                item.status_icon.set_child(Some(&spinner));
                item.status_text
                    .set_label(&tr::tr!("Checking for changes..."));
                update_sync_stats_ui(
                    &directory_map,
                    &remote.name,
                    &sync_dir,
                    &stats.borrow(),
                    None,
                );
                // Dropping this is important, otherwise the pointer borrow might last a lot
                // longer and other parts of the code won't be able to get a pointer to the
                // directory indexmap.
                drop(item_ptr);

                // Add an error for reporting in the UI.
                let please_resolve_msg_tr = tr::tr!("Please resolve the reported syncing issues.");
                let please_resolve_msg = " ".to_owned() + &please_resolve_msg_tr;
                let add_error = glib::clone!(@strong db, @strong server_rows, @strong directory_map, @strong remote, @strong sync_dir, @strong sync_errors_count, @strong please_resolve_msg, @strong handle, @strong stats => move |error: SyncError| {
                    set_server_status(&server_rows, &remote.name, ServerStatus::Broken);
                    {
                        let mut stats_ref = stats.borrow_mut();
                        stats_ref.errors += 1;
                        if matches!(error, SyncError::BothMoreCurrent(_, _)) {
                            stats_ref.conflicts += 1;
                        }
                    }
                    update_sync_stats_ui(
                        &directory_map,
                        &remote.name,
                        &sync_dir,
                        &stats.borrow(),
                        None,
                    );
                    let path_pair = (sync_dir.local_path.clone(), sync_dir.remote_path.clone());
                    let ui_item = error.generate_ui();
                    let ui_item_listbox = ListBoxRow::builder().child(&ui_item).build();

                    // Generate the callback.
                    let gesture = GestureClick::new();
                    gesture.connect_released(glib::clone!(@strong directory_map, @strong remote, @strong path_pair, @strong db, @strong error, @weak ui_item, @weak ui_item_listbox, @strong please_resolve_msg => move |_, _, _, _| {
                        ui_item.set_sensitive(false);
                        let remove_ui_item = glib::clone!(@strong directory_map, @strong remote, @strong path_pair, @strong error, @weak ui_item_listbox, @strong please_resolve_msg => move || {
                            let mut ptr = directory_map.get_mut_ref();
                            let item = ptr.get_mut(&remote.name).unwrap().get_mut(&path_pair).unwrap();

                            // Update the error brief on the main page.
                            let error_text = item.error_status_text.text().to_string();
                            let new_num_errors = error_text.split_whitespace().next().unwrap_or("0").parse::<i32>().unwrap() - 1;
                            if new_num_errors == 0 {
                                item.error_status_text.set_label("");
                                let label_text = match item.status_text.text().as_str().strip_suffix(&please_resolve_msg) {
                                    Some(text) => text.to_string(),
                                    None => item.status_text.text().to_string()
                                };
                                item.status_text.set_label(&label_text);

                            } else {
                                let error_string = tr::tr!("{} errors found. ", new_num_errors);
                                item.error_status_text.set_label(&error_string);
                            }

                            (item.update_error_ui)();

                            // Update the sync dir's page and our code.
                            item.error_items.remove(&error).unwrap();
                            item.error_list.remove(&ui_item_listbox);
                        });

                        match &error {
                            SyncError::General(_, _) => {
                                let dialog = MessageDialog::builder()
                                    .text(&tr::tr!("Would you like to dismiss this error?"))
                                    .buttons(ButtonsType::YesNo)
                                    .build();
                                dialog.connect_close_request(glib::clone!(@strong ui_item => move |_| {
                                    ui_item.set_sensitive(true);
                                    Inhibit(false)
                                }));
                                dialog.connect_response(glib::clone!(@strong directory_map, @strong remote, @strong path_pair, @weak ui_item, @strong error, @strong remove_ui_item => move |dialog, resp| {
                                    match resp {
                                        ResponseType::Yes => {
                                            remove_ui_item();
                                        },
                                        ResponseType::No => {
                                            ui_item.set_sensitive(true);
                                        },
                                        _ => return,
                                    }

                                    dialog.close();
                                }));
                                dialog.show();
                            },
                            SyncError::BothMoreCurrent(local_item, remote_item) => {
                                let local_item_formatted = util::fmt_home(local_item);
                                let local_path = Path::new(&local_item);
                                let sync_local_to_remote = glib::clone!(@strong remote, @strong local_item_formatted, @strong local_item, @strong remote_item => move || {
                                    if let Err(err) = rclone::sync::copy_to_remote(&local_item, &remote.name, &remote_item) {
                                        gtk_util::show_error(&tr::tr!("Failed to sync '{}' to '{}' on remote.", local_item_formatted, remote_item), Some(&err.error));
                                        Err(())
                                    } else {
                                        Ok(())
                                    }
                                });
                                let sync_remote_to_local = glib::clone!(@strong remote, @strong local_item_formatted, @strong local_item, @strong remote_item => move || {
                                    if let Err(err) = rclone::sync::copy_to_local(&local_item, &remote.name, &remote_item) {
                                        gtk_util::show_error(&tr::tr!("Failed to sync '{}' on remote to '{}'.", remote_item, local_item_formatted), Some(&err.error));
                                        Err(())
                                    } else {
                                        Ok(())
                                    }
                                });
                                let local_item = local_item.clone();
                                let update_db_item = glib::clone!(@strong db, @strong remote, @strong local_item, @strong remote_item => move || {
                                    let local_timestamp = Path::new(&local_item).metadata().unwrap().modified().unwrap().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
                                    let remote_timestamp = rclone::sync::stat(&remote.name, &remote_item).unwrap().unwrap().mod_time.unix_timestamp();
                                    let mut active_model: SyncItemsActiveModel = util::await_future(SyncItemsEntity::find()
                                        .filter(SyncItemsColumn::LocalPath.eq(local_item.clone()))
                                        .filter(SyncItemsColumn::RemotePath.eq(remote_item.clone()))
                                        .one(&db)
                                    ).unwrap()
                                    .unwrap()
                                    .into();
                                    active_model.last_local_timestamp = ActiveValue::set(local_timestamp.try_into().unwrap());
                                    active_model.last_remote_timestamp = ActiveValue::Set(remote_timestamp.try_into().unwrap());
                                    util::await_future(active_model.update(&db)).unwrap();
                                });
                                let rclone_remote_item = match rclone::sync::stat(&remote.name, remote_item) {
                                    Ok(item) => item,
                                    Err(err) => {
                                        gtk_util::show_error(
                                            &tr::tr!("Unable to fetch data for '{}' from the remote.", remote_item),
                                            Some(&err.error)
                                        );
                                        return;
                                    }
                                };

                                // If neither the local item or the remote item exist anymore, this error is no longer relevant.
                                if !local_path.exists() && rclone_remote_item.is_none() {
                                    gtk_util::show_error(&tr::tr!("File Update"), Some(&tr::tr!("Neither the local item or remote item exists anymore. This error will now be removed.")));
                                    remove_ui_item();
                                    return;
                                // Otherwise if only the local exists, use that.
                                } else if local_path.exists() && rclone_remote_item.is_none() {
                                    gtk_util::show_error(&tr::tr!("File Update"), Some(&tr::tr!("Only the local item exists now, so it will be synced to the remote.")));
                                    if sync_local_to_remote().is_ok() {
                                        update_db_item();
                                        remove_ui_item();
                                        return;
                                    }
                                // Otherwise if only the remote exists, use that.
                                } else if !local_path.exists() && rclone_remote_item.is_some() {
                                    gtk_util::show_error(&tr::tr!("File Update"), Some(&tr::tr!("Only the remote item exists now, so it will be synced to the local machine.")));
                                    if sync_remote_to_local().is_ok() {
                                        update_db_item();
                                        remove_ui_item();
                                        return;
                                    }
                                }

                                let dialog = MessageDialog::builder()
                                    .text(
                                        &tr::tr!("Both the local item '{}' and remote item '{}' have been updated since the last sync.", local_item_formatted, remote_item)
                                    )
                                    .secondary_text(&tr::tr!("Which item would you like to keep?"))
                                    .build();
                                dialog.add_button(&tr::tr!("Local"), ResponseType::Other(0));
                                dialog.add_button(&tr::tr!("Remote"), ResponseType::Other(1));
                                dialog.connect_close_request(glib::clone!(@strong ui_item => move |_| {
                                    ui_item.set_sensitive(true);
                                    Inhibit(false)
                                }));
                                dialog.connect_response(glib::clone!(@strong directory_map, @strong remote, @strong path_pair, @weak ui_item, @strong error, @strong local_item, @strong remote_item, @strong local_path, @strong rclone_remote_item, @strong sync_local_to_remote, @strong sync_remote_to_local => move |dialog, resp| {
                                    match resp {
                                        ResponseType::Other(0) => {
                                            if sync_local_to_remote().is_ok() {
                                                update_db_item();
                                                remove_ui_item();
                                            }
                                        },
                                        ResponseType::Other(1) => {
                                            if sync_remote_to_local().is_ok() {
                                                update_db_item();
                                                remove_ui_item();
                                            }
                                        },
                                        ResponseType::Other(_) => unreachable!(),
                                        _ => return
                                    }

                                    dialog.close();
                                }));

                                dialog.show();
                            }
                        }
                    }));
                    ui_item.add_controller(&gesture);

                    // If we have zero errors now, remove the warning icon.
                    if sync_errors_count() == 0 {
                        handle.update(|tray| tray.set_syncing());
                    }

                    // Report the brief on the number of errors.
                    let mut ptr = directory_map.get_mut_ref();
                    let item = ptr
                        .get_mut(&remote.name)
                        .unwrap()
                        .get_mut(&path_pair)
                        .unwrap();

                    let error_text = item.error_status_text.text().to_string();
                    let new_num_errors = error_text.split_whitespace().next().unwrap_or("0").parse::<i32>().unwrap() + 1;

                    let error_string = if new_num_errors == 1 {
                        tr::tr!("1 error found.")
                    } else {
                        tr::tr!("{} errors found.", new_num_errors)
                    };
                    item.error_status_text.set_label(&(error_string + " "));

                    // Add the error to the UI.
                    item.error_list.append(&ui_item_listbox);
                    item.error_items.insert(error, ui_item);
                    (item.update_error_ui)();

                    // Set the tray icon to show the warning icon.
                    handle.update(|tray| tray.set_warning());
                });

                // A vector of local/remote sync item pairs to make sure we don't sync anything
                // twice between 'sync_local_directory' and 'sync_remote_directory' below. It
                // also prevents errors from showing up twice when they occur. We have to wrap
                // this in a [`RefCell`] to avoid some borrow checker issues with multiple
                // mutable closures needing access to this.
                let synced_items: RefCell<Vec<(String, String)>> = RefCell::new(vec![]);

                // Get any pending deletion requests and process them.
                let process_deletion_requests = glib::clone!(@strong db, @weak stack, @weak server_list, @strong server_rows, @strong server_status_checked, @strong directory_map, @strong remote_deletion_queue, @strong sync_dir_deletion_queue => move || {
                    let mut dmap = directory_map.get_mut_ref();
                    let mut remote_queue = remote_deletion_queue.get_mut_ref();
                    let mut dir_queue = sync_dir_deletion_queue.get_mut_ref();

                    // Process directory deletions.
                    while !dir_queue.is_empty() {
                        let queue_item = dir_queue.remove(0);
                        let dir_pair = (queue_item.1.clone(), queue_item.2.clone());
                        let ui_item = dmap.get(&queue_item.0).unwrap().get(&dir_pair).unwrap();

                        // Remove the item from the UI.
                        ui_item.parent_list.remove(&ui_item.container);

                        // Remove the item from the directory map.
                        dmap.get_mut(&queue_item.0).unwrap().remove(&dir_pair).unwrap();

                        // Remove the item from the database.
                        util::await_future(async {
                            let sync_dir = SyncDirsEntity::find()
                                .filter(SyncDirsColumn::LocalPath.eq(queue_item.1.clone()))
                                .filter(SyncDirsColumn::RemotePath.eq(queue_item.2.clone()))
                                .one(&db)
                                .await
                                .unwrap()
                                .unwrap();

                            SyncItemsEntity::delete_many()
                                .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                                .exec(&db)
                                .await
                                .unwrap();
                            sync_dir.delete(&db).await.unwrap();
                        });
                    }

                    // Process remote deletions.
                    while !remote_queue.is_empty() {
                        let remote_name = remote_queue.remove(0);

                        // Remove the item from the UI.
                        let child = stack.child_by_name(&remote_name).unwrap();
                        stack.remove(&child);
                        if let Some(server_row) = server_rows.borrow_mut().shift_remove(&remote_name) {
                            server_list.remove(&server_row.row);
                        }
                        server_status_checked.borrow_mut().remove(&remote_name);

                        // Delete all related database entries.
                        util::await_future(async {
                            let db_remote = RemotesEntity::find()
                                .filter(RemotesColumn::Name.eq(remote_name.clone()))
                                .one(&db)
                                .await
                                .unwrap()
                                .unwrap();
                            let sync_dirs = SyncDirsEntity::find()
                                .filter(SyncDirsColumn::RemoteId.eq(db_remote.id))
                                .all(&db)
                                .await
                                .unwrap();

                            for sync_dir in sync_dirs {
                                SyncItemsEntity::delete_many()
                                    .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                                    .exec(&db)
                                    .await
                                    .unwrap();
                                sync_dir.delete(&db).await.unwrap();
                            }

                            db_remote.delete(&db).await.unwrap();
                        });

                        // Delete the Rclone config.
                        rclone::sync::delete_config(&remote_name).unwrap();
                    }
                });

                // Sync a local directory. This is implemented as a function instead of a
                // closure so that it can be called recursively.
                //
                // Returning an [`Err<()>`] means we this directory has to stop being synced
                // because it was in the deletion queue. Any other error should return an
                // [`Ok<()>`].
                #[allow(clippy::too_many_arguments)]
                fn sync_local_directory<
                    F1: Fn(SyncError) + Clone,
                    F2: Fn() + Clone,
                    F3: Fn() + Clone,
                >(
                    local_dir: &Path,
                    remote: &RemotesModel,
                    sync_dir: &SyncDirsModel,
                    db: &DatabaseConnection,
                    directory_map: &DirectoryMap,
                    stats: &SyncStatsRef,
                    synced_items: &RefCell<Vec<(String, String)>>,
                    add_error: F1,
                    check_open_requests: F2,
                    process_deletion_requests: F3,
                ) {
                    process_deletion_requests();

                    let dir_string = local_dir.to_str().unwrap().to_owned();
                    let update_ui_progress = |dir: &str| {
                        // If this directory no longer exists in the database (i.e. from being
                        // deleted from the `sync_dir_deletion_queue`), then do nothing.
                        if !sync_dir.exists(db) {
                            return;
                        }

                        let ptr = directory_map.get_ref();
                        let dir_pair = (sync_dir.local_path.clone(), sync_dir.remote_path.clone());
                        let item = ptr.get(&remote.name).unwrap().get(&dir_pair).unwrap();
                        let status_string =
                            tr::tr!("Checking '{}' for changes...", util::fmt_home(dir));
                        item.status_text.set_label(&status_string);
                    };
                    update_ui_progress(&dir_string);
                    let directory = match fs::read_dir(local_dir) {
                        Ok(ok_dir) => ok_dir,
                        Err(err) => {
                            add_error(SyncError::General(dir_string, err.to_string()));
                            return;
                        }
                    };

                    // Get the list of ignore globs.
                    let ignore_file_string =
                        format!("{}/{}", sync_dir.local_path, FILE_IGNORE_NAME);
                    let ignore_file_path = Path::new(&ignore_file_string);
                    let ignore_globs = if ignore_file_path.exists() {
                        let _lock = FileLock::lock(
                            &ignore_file_string,
                            true,
                            FileOptions::new().write(true).read(true),
                        )
                        .unwrap();
                        let file_content = fs::read_to_string(ignore_file_path).unwrap();
                        let mut globs = vec![];

                        for line in file_content.lines() {
                            if let Ok(pattern) = glob::Pattern::new(line) {
                                globs.push(pattern);
                            }
                        }

                        globs
                    } else {
                        vec![]
                    };

                    for item in directory {
                        // If a close request was sent in, stop syncing this remote so we can quit
                        // the application in the 'main loop.
                        if *(*CLOSE_REQUEST).lock().unwrap() {
                            break;
                        }

                        // Check for open requests.
                        check_open_requests();

                        // If this directory no longer exists in the database (i.e. from being
                        // deleted from the `sync_dir_deletion_queue`), stop processing and return.
                        if !sync_dir.exists(db) {
                            break;
                        }

                        if let Err(err) = item {
                            add_error(SyncError::General(dir_string.clone(), err.to_string()));
                            continue;
                        }
                        let item = item.unwrap();
                        let local_path = item.path().to_str().unwrap().to_owned();
                        let local_scanned = {
                            let mut stats_ref = stats.borrow_mut();
                            stats_ref.local_scanned += 1;
                            stats_ref.local_scanned
                        };
                        maybe_process_ui_events(local_scanned);

                        // The path from the root of the remote.
                        let remote_path = {
                            let local_path_stripped = local_path
                                .strip_prefix(&format!("{}/", sync_dir.local_path))
                                .unwrap();
                            let stripped_path = match local_path_stripped.strip_suffix('/') {
                                Some(string) => string,
                                None => local_path_stripped,
                            };

                            if sync_dir.remote_path.is_empty() {
                                stripped_path.to_owned()
                            } else {
                                sync_dir.remote_path.clone() + "/" + stripped_path
                            }
                        };
                        // The above path, with `sync_dir.remote_path` stripped from it.
                        let stripped_remote_path =
                            if remote_path.contains('/') && sync_dir.remote_path.contains('/') {
                                remote_path
                                    .strip_prefix(&format!("{}/", sync_dir.remote_path))
                                    .unwrap()
                                    .to_owned()
                            } else {
                                remote_path.clone()
                            };

                        update_ui_progress(&local_path);
                        // If this item matches the ignore list, don't sync it.
                        if ignore_globs
                            .iter()
                            .filter(|pattern| pattern.matches(&stripped_remote_path))
                            .count()
                            > 0
                        {
                            stats.borrow_mut().skipped += 1;
                            update_sync_stats_ui(
                                directory_map,
                                &remote.name,
                                sync_dir,
                                &stats.borrow(),
                                Some(&local_path),
                            );
                            continue;
                        }

                        let processed = {
                            let mut stats_ref = stats.borrow_mut();
                            stats_ref.processed += 1;
                            stats_ref.processed
                        };
                        maybe_process_ui_events(processed);
                        update_sync_stats_ui(
                            directory_map,
                            &remote.name,
                            sync_dir,
                            &stats.borrow(),
                            Some(&local_path),
                        );
                        synced_items
                            .borrow_mut()
                            .push((local_path.clone(), remote_path.clone()));

                        let get_local_file_timestamp = || {
                            item.metadata()
                                .unwrap()
                                .modified()
                                .unwrap()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                        };
                        let local_utc_timestamp = get_local_file_timestamp();
                        let remote_item = match rclone::sync::stat(&remote.name, &remote_path) {
                            Ok(item) => item,
                            Err(err) => {
                                add_error(SyncError::General(remote_path.clone(), err.error));
                                continue;
                            }
                        };
                        let remote_utc_timestamp = remote_item
                            .as_ref()
                            .map(|item| item.mod_time.unix_timestamp());
                        let db_item = util::await_future(
                            SyncItemsEntity::find()
                                .filter(SyncItemsColumn::LocalPath.eq(local_path.clone()))
                                .filter(SyncItemsColumn::RemotePath.eq(remote_path.clone()))
                                .one(db),
                        )
                        .unwrap();

                        // Push the item to the remote. Returns the
                        // [`crate::rclone::sync::RcloneRemoteItem`] of the item on the remote, or
                        // an [`Err<()>`] if an issue occurred (all errors are automatically added
                        // via `add_errors`).
                        let push_local_to_remote = || -> Result<rclone::RcloneRemoteItem, ()> {
                            let file_type = item.file_type().unwrap();

                            if let Some(rclone_item) = &remote_item {
                                let same_type = file_type.is_dir() && rclone_item.is_dir;

                                if !same_type {
                                    if let Err(err) =
                                        rclone::sync::purge(&remote.name, &remote_path)
                                    {
                                        add_error(SyncError::General(
                                            remote_path.clone(),
                                            err.error,
                                        ));
                                        return Err(());
                                    }
                                }
                            }

                            if file_type.is_dir() {
                                if let Err(err) = rclone::sync::mkdir(&remote.name, &remote_path) {
                                    add_error(SyncError::General(remote_path.clone(), err.error));
                                    return Err(());
                                }
                                sync_local_directory(
                                    &item.path(),
                                    remote,
                                    sync_dir,
                                    db,
                                    directory_map,
                                    stats,
                                    synced_items,
                                    add_error.clone(),
                                    check_open_requests.clone(),
                                    process_deletion_requests.clone(),
                                );
                                update_ui_progress(&local_path);
                            } else if let Err(err) = rclone::sync::copy_to_remote(
                                &local_path,
                                &remote.name,
                                &remote_path,
                            ) {
                                add_error(SyncError::General(local_path.clone(), err.error));
                                return Err(());
                            }

                            stats.borrow_mut().uploaded += 1;
                            update_sync_stats_ui(
                                directory_map,
                                &remote.name,
                                sync_dir,
                                &stats.borrow(),
                                Some(&local_path),
                            );
                            Ok(rclone::sync::stat(&remote.name, &remote_path)
                                .unwrap()
                                .unwrap())
                        };
                        // Pull the item from the remote.
                        let pull_remote_to_local = || -> Result<(), ()> {
                            let file_type = item.file_type().unwrap();
                            let same_type =
                                file_type.is_dir() && remote_item.as_ref().unwrap().is_dir;

                            if !same_type {
                                if file_type.is_dir()
                                    && let Err(err) = fs::remove_dir_all(item.path())
                                {
                                    add_error(SyncError::General(
                                        local_path.clone(),
                                        err.to_string(),
                                    ));
                                    return Err(());
                                } else if let Err(err) = fs::remove_file(item.path()) {
                                    add_error(SyncError::General(
                                        local_path.clone(),
                                        err.to_string(),
                                    ));
                                    return Err(());
                                }
                            }

                            if file_type.is_dir() {
                                sync_local_directory(
                                    &item.path(),
                                    remote,
                                    sync_dir,
                                    db,
                                    directory_map,
                                    stats,
                                    synced_items,
                                    add_error.clone(),
                                    check_open_requests.clone(),
                                    process_deletion_requests.clone(),
                                );
                                update_ui_progress(&local_path);
                            } else if let Err(err) =
                                rclone::sync::copy_to_local(&local_path, &remote.name, &remote_path)
                            {
                                add_error(SyncError::General(remote_path.clone(), err.error));
                                return Err(());
                            }

                            stats.borrow_mut().downloaded += 1;
                            update_sync_stats_ui(
                                directory_map,
                                &remote.name,
                                sync_dir,
                                &stats.borrow(),
                                Some(&local_path),
                            );
                            Ok(())
                        };
                        // Delete this item from the database.
                        let delete_db_entry = || {
                            util::await_future(async {
                                SyncItemsEntity::find()
                                    .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                                    .filter(SyncItemsColumn::LocalPath.eq(local_path.clone()))
                                    .filter(SyncItemsColumn::RemotePath.eq(remote_path.clone()))
                                    .one(db)
                                    .await
                                    .unwrap()
                                    .unwrap()
                                    .delete(db)
                                    .await
                                    .unwrap()
                            })
                        };

                        // If we have a record of the last sync, use that to aid in timestamp
                        // checks.
                        if let Some(db_model) = db_item {
                            let update_db_item = |local_timestamp, remote_timestamp| {
                                let mut active_model: SyncItemsActiveModel =
                                    db_model.clone().into();
                                active_model.last_local_timestamp =
                                    ActiveValue::Set(local_timestamp);
                                active_model.last_remote_timestamp =
                                    ActiveValue::Set(remote_timestamp);
                                util::await_future(active_model.update(db)).unwrap();
                            };

                            // Both items are more current than at the last transaction - we need to
                            // let the user decide which to keep.
                            // Since `db_model.last_sync_timestamp` is an `i32`, we should be able
                            // to safely convert it to an `i64` and `u64`.
                            if local_utc_timestamp > db_model.last_local_timestamp as u64
                                && let Some(remote_timestamp) = remote_utc_timestamp
                                && remote_timestamp > db_model.last_remote_timestamp as i64
                            {
                                // Only add the error if one of the items is not a directory -
                                // there's no point in saying both directories are more current, and
                                // it's probably because one of the items in the directory got
                                // updated anyway.
                                if let Some(r_item) = remote_item
                                    && (!item.path().is_dir() || !r_item.is_dir)
                                {
                                    add_error(SyncError::BothMoreCurrent(
                                        local_path.clone(),
                                        remote_path.clone(),
                                    ));
                                }
                            // The local item is more recent.
                            } else if local_utc_timestamp > db_model.last_local_timestamp as u64 {
                                if let Ok(rclone_item) = push_local_to_remote() {
                                    update_db_item(
                                        get_local_file_timestamp().try_into().unwrap(),
                                        rclone_item.mod_time.unix_timestamp().try_into().unwrap(),
                                    );
                                    continue;
                                } else {
                                    continue;
                                }
                            // The remote item is more recent.
                            } else if let Some(remote_timestamp) = remote_utc_timestamp
                                && remote_timestamp > db_model.last_remote_timestamp as i64
                            {
                                if pull_remote_to_local().is_err() {
                                    continue;
                                } else {
                                    update_db_item(
                                        get_local_file_timestamp().try_into().unwrap(),
                                        remote_timestamp.try_into().unwrap(),
                                    );
                                }
                            // The item is missing from the remote, but the last
                            // recorded timestamp for the local item is still
                            // the same. This means the item got deleted on the
                            // server, and we need to reflect such locally.
                            } else if remote_item.is_none()
                                && local_utc_timestamp == db_model.last_local_timestamp as u64
                            {
                                if item.path().is_dir() {
                                    if let Err(err) = fs::remove_dir_all(&local_path) {
                                        add_error(SyncError::General(
                                            local_path.clone(),
                                            err.to_string(),
                                        ));
                                        continue;
                                    }
                                } else if let Err(err) = fs::remove_file(&local_path) {
                                    add_error(SyncError::General(
                                        local_path.clone(),
                                        err.to_string(),
                                    ));
                                    continue;
                                }

                                stats.borrow_mut().deleted += 1;
                                update_sync_stats_ui(
                                    directory_map,
                                    &remote.name,
                                    sync_dir,
                                    &stats.borrow(),
                                    Some(&local_path),
                                );
                                delete_db_entry();
                                continue;
                            // Both the local and remote item remain unchanged -
                            // do nothing.
                            } else if local_utc_timestamp == db_model.last_local_timestamp as u64
                                && let Some(remote_timestamp) = remote_utc_timestamp
                                && remote_timestamp == db_model.last_remote_timestamp as i64
                            {
                                continue;
                            // Every possible scenario should have been covered
                            // above, so panic if not.
                            } else {
                                unreachable!();
                            }
                        // Otherwise just check the local timestamps against
                        // those on the remote, and record our new transaction
                        // in the database.
                        } else {
                            // If the timestamp exists, then the remote item did, so check
                            // timestamps.
                            if let Some(remote_timestamp) = remote_utc_timestamp {
                                if local_utc_timestamp > remote_timestamp as u64 {
                                    if push_local_to_remote().is_err() {
                                        continue;
                                    }
                                } else if pull_remote_to_local().is_err() {
                                    continue;
                                }
                            // Otherwise the remote item didn't exist, so just
                            // sync our local copy.
                            } else if push_local_to_remote().is_err() {
                                continue;
                            }

                            // The remote item is now guaranteed to exist, so fetch it.
                            let remote_item_safe =
                                match rclone::sync::stat(&remote.name, &remote_path) {
                                    Ok(item) => item.unwrap(),
                                    Err(err) => {
                                        add_error(SyncError::General(
                                            remote_path.clone(),
                                            err.error,
                                        ));
                                        continue;
                                    }
                                };
                            match rclone::sync::stat(&remote.name, &remote_path) {
                                Ok(item) => item.unwrap(),
                                Err(err) => {
                                    add_error(SyncError::General(remote_path.clone(), err.error));
                                    continue;
                                }
                            };

                            // Record the current transaction's timestamps in the database.
                            util::await_future(
                                SyncItemsActiveModel {
                                    sync_dir_id: ActiveValue::Set(sync_dir.id),
                                    local_path: ActiveValue::Set(local_path.clone()),
                                    remote_path: ActiveValue::Set(remote_path.clone()),
                                    last_local_timestamp: ActiveValue::Set(
                                        local_utc_timestamp.try_into().unwrap(),
                                    ),
                                    last_remote_timestamp: ActiveValue::Set(
                                        remote_item_safe
                                            .mod_time
                                            .unix_timestamp()
                                            .try_into()
                                            .unwrap(),
                                    ),
                                    ..Default::default()
                                }
                                .insert(db),
                            )
                            .unwrap();
                        }
                    }
                }

                // Sync a remote directory. It's implemented as a function because of the same
                // logic for `fn sync_local_directory` above.
                // - NOTE: `remote_dir` should be: 1. the path with any `/` prefix/suffix
                //   removed 2. the full path from the root of the remote server.
                #[allow(clippy::too_many_arguments)]
                fn sync_remote_directory<
                    F1: Fn(SyncError) + Clone,
                    F2: Fn() + Clone,
                    F3: Fn() + Clone,
                >(
                    remote_dir: &str,
                    remote: &RemotesModel,
                    sync_dir: &SyncDirsModel,
                    db: &DatabaseConnection,
                    directory_map: &DirectoryMap,
                    stats: &SyncStatsRef,
                    synced_items: &RefCell<Vec<(String, String)>>,
                    add_error: F1,
                    check_open_requests: F2,
                    process_deletion_requests: F3,
                ) {
                    process_deletion_requests();

                    let ignore_file_string =
                        format!("{}/{}", sync_dir.local_path, FILE_IGNORE_NAME);
                    let ignore_file_path = Path::new(&ignore_file_string);
                    let ignore_globs = if ignore_file_path.exists() {
                        let _lock = FileLock::lock(
                            ignore_file_path,
                            true,
                            FileOptions::new().write(true).read(true),
                        )
                        .unwrap();
                        let file_content = fs::read_to_string(ignore_file_path).unwrap();
                        let mut globs = vec![];

                        for line in file_content.lines() {
                            if let Ok(pattern) = glob::Pattern::new(line) {
                                globs.push(pattern);
                            }
                        }

                        globs
                    } else {
                        vec![]
                    };
                    let update_ui_progress = |dir: &str| {
                        // If this directory no longer exists in the database (i.e. from being
                        // deleted from the `sync_dir_deletion_queue`, do nothing).
                        if !sync_dir.exists(db) {
                            return;
                        }

                        let ptr = directory_map.get_ref();
                        let dir_pair = (sync_dir.local_path.clone(), sync_dir.remote_path.clone());
                        let item = ptr.get(&remote.name).unwrap().get(&dir_pair).unwrap();
                        let status_string = tr::tr!("Checking '{}' on remote for changes...", dir);
                        item.status_text.set_label(&status_string);
                    };
                    update_ui_progress(remote_dir);
                    let items = match rclone::sync::list(
                        &remote.name,
                        remote_dir,
                        false,
                        RcloneListFilter::All,
                    ) {
                        Ok(ok_items) => ok_items,
                        Err(err) => {
                            add_error(SyncError::General(remote_dir.to_owned(), err.error));
                            return;
                        }
                    };

                    for item in items {
                        let remote_scanned = {
                            let mut stats_ref = stats.borrow_mut();
                            stats_ref.remote_scanned += 1;
                            stats_ref.remote_scanned
                        };
                        maybe_process_ui_events(remote_scanned);
                        // If a close request was sent in, stop syncing this remote so we can quit
                        // the application in the 'main loop.
                        if *(*CLOSE_REQUEST).lock().unwrap() {
                            break;
                        }

                        // Check for open requests.
                        check_open_requests();

                        // If this directory no longer exists in the database (i.e. from being
                        // deleted from the `sync_dir_deletion_queue`), stop processing and return.
                        if !sync_dir.exists(db) {
                            break;
                        }

                        // If this item matches the ignore filter, don't sync it.
                        if ignore_globs
                            .iter()
                            .filter(|pattern| pattern.matches(&item.path))
                            .count()
                            > 0
                        {
                            stats.borrow_mut().skipped += 1;
                            update_sync_stats_ui(
                                directory_map,
                                &remote.name,
                                sync_dir,
                                &stats.borrow(),
                                Some(&item.path),
                            );
                            continue;
                        }

                        let remote_path_string = item.path.clone();
                        let local_path_string = format!(
                            "{}/{}",
                            sync_dir.local_path,
                            item.path.strip_prefix(&sync_dir.remote_path).unwrap()
                        );
                        update_ui_progress(&remote_path_string);

                        // If we've already synced this directory from `fn sync_local_directory`
                        // above, don't sync it again.
                        if synced_items
                            .borrow()
                            .contains(&(local_path_string.clone(), remote_path_string.clone()))
                        {
                            stats.borrow_mut().skipped += 1;
                            update_sync_stats_ui(
                                directory_map,
                                &remote.name,
                                sync_dir,
                                &stats.borrow(),
                                Some(&remote_path_string),
                            );
                            continue;
                        }

                        let processed = {
                            let mut stats_ref = stats.borrow_mut();
                            stats_ref.processed += 1;
                            stats_ref.processed
                        };
                        maybe_process_ui_events(processed);
                        update_sync_stats_ui(
                            directory_map,
                            &remote.name,
                            sync_dir,
                            &stats.borrow(),
                            Some(&remote_path_string),
                        );
                        let local_path = Path::new(&local_path_string);
                        let remote_timestamp = item.mod_time.unix_timestamp();
                        let get_local_file_timestamp = || {
                            local_path.metadata().ok().map(|metadata| {
                                metadata
                                    .modified()
                                    .unwrap()
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .unwrap()
                                    .as_secs()
                            })
                        };
                        let local_timestamp = get_local_file_timestamp();
                        let db_item = util::await_future(
                            SyncItemsEntity::find()
                                .filter(SyncItemsColumn::LocalPath.eq(local_path_string.clone()))
                                .filter(SyncItemsColumn::RemotePath.eq(remote_path_string.clone()))
                                .one(db),
                        )
                        .unwrap();

                        // Push the item from the local machine to the remote machine. Returns the
                        // timestamp of the new file on the remote. Returns the
                        // [`crate::rclone::sync::RcloneRemoteItem`] of the item on the remote, or
                        // an [`Err<()>`] if an issue occurred (all errors are automatically added
                        // via `add_errors`).
                        let push_local_to_remote = || {
                            if local_path.is_dir() {
                                if !item.is_dir {
                                    if let Err(err) =
                                        rclone::sync::delete(&remote.name, &remote_path_string)
                                    {
                                        add_error(SyncError::General(
                                            remote_path_string.clone(),
                                            err.error,
                                        ));
                                        return Err(());
                                    }

                                    if let Err(err) =
                                        rclone::sync::mkdir(&remote.name, &remote_path_string)
                                    {
                                        add_error(SyncError::General(
                                            remote_path_string.clone(),
                                            err.error,
                                        ));
                                        return Err(());
                                    }
                                }

                                sync_remote_directory(
                                    &item.path,
                                    remote,
                                    sync_dir,
                                    db,
                                    directory_map,
                                    stats,
                                    synced_items,
                                    add_error.clone(),
                                    check_open_requests.clone(),
                                    process_deletion_requests.clone(),
                                );
                                update_ui_progress(&remote_path_string);
                            } else {
                                if item.is_dir {
                                    if let Err(err) =
                                        rclone::sync::purge(&remote.name, &remote_path_string)
                                    {
                                        add_error(SyncError::General(
                                            remote_path_string.clone(),
                                            err.error,
                                        ));
                                        return Err(());
                                    }
                                }

                                if let Err(err) = rclone::sync::copy_to_remote(
                                    &local_path_string,
                                    &remote.name,
                                    &remote_path_string,
                                ) {
                                    add_error(SyncError::General(
                                        remote_path_string.clone(),
                                        err.error,
                                    ));
                                    return Err(());
                                }
                            }

                            stats.borrow_mut().uploaded += 1;
                            update_sync_stats_ui(
                                directory_map,
                                &remote.name,
                                sync_dir,
                                &stats.borrow(),
                                Some(&remote_path_string),
                            );
                            Ok(rclone::sync::stat(&remote.name, &remote_path_string)
                                .unwrap()
                                .unwrap())
                        };

                        // Pull the item from the remote to the local machine.
                        let pull_remote_to_local = || {
                            // Make sure file types match up.
                            if local_path.exists() {
                                if local_path.is_dir() && !item.is_dir {
                                    if let Err(err) = fs::remove_dir_all(local_path) {
                                        add_error(SyncError::General(
                                            local_path_string.clone(),
                                            err.to_string(),
                                        ));
                                        return Err(());
                                    }
                                } else if !local_path.is_dir() && item.is_dir {
                                    if let Err(err) = fs::remove_file(local_path) {
                                        add_error(SyncError::General(
                                            local_path_string.clone(),
                                            err.to_string(),
                                        ));
                                        return Err(());
                                    }

                                    if let Err(err) = fs::create_dir(local_path) {
                                        add_error(SyncError::General(
                                            local_path_string.clone(),
                                            err.to_string(),
                                        ));
                                        return Err(());
                                    }
                                }
                            }

                            if item.is_dir {
                                if !local_path.exists()
                                    && let Err(err) = fs::create_dir(local_path)
                                {
                                    add_error(SyncError::General(
                                        local_path_string.clone(),
                                        err.to_string(),
                                    ));
                                    return Err(());
                                }

                                sync_remote_directory(
                                    &item.path,
                                    remote,
                                    sync_dir,
                                    db,
                                    directory_map,
                                    stats,
                                    synced_items,
                                    add_error.clone(),
                                    check_open_requests.clone(),
                                    process_deletion_requests.clone(),
                                );
                                update_ui_progress(&remote_path_string);
                            } else if let Err(err) = rclone::sync::copy_to_local(
                                &local_path_string,
                                &remote.name,
                                &remote_path_string,
                            ) {
                                add_error(SyncError::General(
                                    remote_path_string.clone(),
                                    err.error,
                                ));
                                return Err(());
                            }

                            stats.borrow_mut().downloaded += 1;
                            update_sync_stats_ui(
                                directory_map,
                                &remote.name,
                                sync_dir,
                                &stats.borrow(),
                                Some(&remote_path_string),
                            );
                            Ok(())
                        };
                        // Delete this item from the database.
                        let delete_db_entry = || {
                            util::await_future(async {
                                SyncItemsEntity::find()
                                    .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                                    .filter(
                                        SyncItemsColumn::LocalPath.eq(local_path_string.clone()),
                                    )
                                    .filter(
                                        SyncItemsColumn::RemotePath.eq(remote_path_string.clone()),
                                    )
                                    .one(db)
                                    .await
                                    .unwrap()
                                    .unwrap()
                                    .delete(db)
                                    .await
                                    .unwrap()
                            })
                        };

                        // If we have a database record, use that in checks.
                        if let Some(db_model) = db_item {
                            let update_db_item = |local_timestamp, remote_timestamp| {
                                let mut active_model: SyncItemsActiveModel =
                                    db_model.clone().into();
                                active_model.last_local_timestamp =
                                    ActiveValue::Set(local_timestamp);
                                active_model.last_remote_timestamp =
                                    ActiveValue::Set(remote_timestamp);
                                util::await_future(active_model.update(db)).unwrap();
                            };

                            // Both items are more recent.
                            if let Some(l_timestamp) = local_timestamp
                                && l_timestamp > db_model.last_local_timestamp as u64
                                && remote_timestamp > db_model.last_remote_timestamp as i64
                            {
                                // Only add the error if one of the items is not a directory -
                                // there's no point in saying both directories are more current, and
                                // it's probably because one of the items in the directory got
                                // updated anyway.
                                if !local_path.is_dir() || !item.is_dir {
                                    add_error(SyncError::BothMoreCurrent(
                                        local_path_string.clone(),
                                        remote_path_string.clone(),
                                    ));
                                }
                                continue;
                            // The local item is more recent.
                            } else if let Some(l_timestamp) = local_timestamp
                                && l_timestamp > db_model.last_local_timestamp as u64
                            {
                                if let Ok(rclone_item) = push_local_to_remote() {
                                    update_db_item(
                                        get_local_file_timestamp().unwrap().try_into().unwrap(),
                                        rclone_item.mod_time.unix_timestamp().try_into().unwrap(),
                                    );
                                    continue;
                                } else {
                                    stats.borrow_mut().deleted += 1;
                                    update_sync_stats_ui(
                                        directory_map,
                                        &remote.name,
                                        sync_dir,
                                        &stats.borrow(),
                                        Some(&remote_path_string),
                                    );
                                    continue;
                                }

                            // The remote item is more recent.
                            } else if remote_timestamp > db_model.last_remote_timestamp as i64 {
                                if pull_remote_to_local().is_err() {
                                    continue;
                                } else {
                                    update_db_item(
                                        get_local_file_timestamp().unwrap().try_into().unwrap(),
                                        remote_timestamp.try_into().unwrap(),
                                    );
                                }

                            // The item is missing locally, but the last
                            // recorded timestamp for the remote item is still
                            // the same. This means the item got deleted
                            // locally, and we need to reflect such on the
                            // server.
                            } else if !local_path.exists()
                                && remote_timestamp == db_model.last_remote_timestamp as i64
                            {
                                if let Err(err) =
                                    rclone::sync::purge(&remote.name, &remote_path_string)
                                {
                                    add_error(SyncError::General(
                                        remote_path_string.clone(),
                                        err.error,
                                    ));
                                    delete_db_entry();
                                    continue;
                                } else {
                                    continue;
                                }

                            // Both the local and remote item remain unchanged -
                            // do nothing.
                            } else if let Some(l_timestamp) = local_timestamp
                                && l_timestamp == db_model.last_local_timestamp as u64
                                && remote_timestamp == db_model.last_remote_timestamp as i64
                            {
                                continue;

                            // Every possible scenario should have been covered
                            // above, so panic if not.
                            } else {
                                unreachable!();
                            }
                        // Otherwise just check the local timestamps against
                        // those on th remote, and record our new transaction in
                        // the database.
                        } else {
                            // If the local timestamp exists, then compare local and remote
                            // timestamps.
                            if let Some(l_timestamp) = local_timestamp {
                                if l_timestamp > remote_timestamp as u64 {
                                    if push_local_to_remote().is_err() {
                                        continue;
                                    }
                                } else if pull_remote_to_local().is_err() {
                                    continue;
                                }

                            // Otherwise the local item didn't exist, so just
                            // sync it from the remote.
                            } else if pull_remote_to_local().is_err() {
                                continue;
                            }
                        }

                        // The local item is now guaranteed to exist. Also fetch the remote's
                        // timestamp in case it got updated above.
                        let l_timestamp = get_local_file_timestamp().unwrap();
                        let r_timestamp =
                            match rclone::sync::stat(&remote.name, &remote_path_string) {
                                Ok(item) => item.unwrap().mod_time.unix_timestamp(),
                                Err(err) => {
                                    add_error(SyncError::General(
                                        remote_path_string.clone(),
                                        err.error,
                                    ));
                                    continue;
                                }
                            };

                        // Record the current transaction's timestamps in the database.
                        util::await_future(
                            SyncItemsActiveModel {
                                sync_dir_id: ActiveValue::Set(sync_dir.id),
                                local_path: ActiveValue::Set(local_path_string.clone()),
                                remote_path: ActiveValue::Set(remote_path_string.clone()),
                                last_local_timestamp: ActiveValue::Set(
                                    l_timestamp.try_into().unwrap(),
                                ),
                                last_remote_timestamp: ActiveValue::Set(
                                    r_timestamp.try_into().unwrap(),
                                ),
                                ..Default::default()
                            }
                            .insert(db),
                        )
                        .unwrap();
                    }
                }

                let actual_sync_started = Instant::now();
                util::log_line(&format!(
                    "sync dir actual sync start remote={} local={} remote_path={}",
                    remote.name, sync_dir.local_path, sync_dir.remote_path
                ));

                sync_local_directory(
                    Path::new(&sync_dir.local_path),
                    &remote,
                    &sync_dir,
                    &db,
                    &directory_map,
                    &stats,
                    &synced_items,
                    &add_error,
                    &check_open_requests,
                    &process_deletion_requests,
                );
                util::log_line(&format!(
                    "sync dir local sync pass finished remote={} local={} processed={} elapsed_ms={}",
                    remote.name,
                    sync_dir.local_path,
                    stats.borrow().processed,
                    actual_sync_started.elapsed().as_millis()
                ));
                sync_remote_directory(
                    &sync_dir.remote_path,
                    &remote,
                    &sync_dir,
                    &db,
                    &directory_map,
                    &stats,
                    &synced_items,
                    &add_error,
                    &check_open_requests,
                    &process_deletion_requests,
                );
                util::log_line(&format!(
                    "sync dir actual sync finished remote={} local={} processed={} errors={} elapsed_ms={}",
                    remote.name,
                    sync_dir.local_path,
                    stats.borrow().processed,
                    stats.borrow().errors,
                    actual_sync_started.elapsed().as_millis()
                ));

                // If a close request was sent in, quit.
                if *(*CLOSE_REQUEST).lock().unwrap() {
                    continue 'main;
                }

                // If this sync directory doesn't exist anymore (from being deleted during
                // `process_deletion_requests` calls in the about two functions), go to the next
                // sync directory.
                if !sync_dir.exists(&db) {
                    continue 'main;
                }

                // Set up the UI for notifying the user that this directory has been synced.
                let item_ptr = directory_map.get_ref();
                let item = item_ptr
                    .get(&remote.name)
                    .unwrap()
                    .get(&(sync_dir.local_path.clone(), sync_dir.remote_path.clone()))
                    .unwrap();
                item.status_icon
                    .set_child(Some(&get_image("object-select-symbolic")));
                let mut finished_text = tr::tr!("Files are synced.");
                if item.error_status_text.text().len() != 0 {
                    finished_text += &please_resolve_msg;
                    item.status_icon
                        .set_child(Some(&get_image("dialog-warning-symbolic")));
                } else {
                    item.status_icon
                        .set_child(Some(&get_image("object-select-symbolic")));
                }
                item.status_text.set_label(&finished_text);
                drop(item_ptr);
                util::log_line(&format!(
                    "sync dir finished remote={} local={} elapsed_ms={}",
                    remote.name,
                    sync_dir.local_path,
                    sync_dir_started.elapsed().as_millis()
                ));
                if stats.borrow().errors > 0 {
                    remote_failed = true;
                }
            }

            if remote_failed {
                server_status_checked
                    .borrow_mut()
                    .insert(remote.name.clone(), false);
                set_server_status(&server_rows, &remote.name, ServerStatus::Broken);
            } else {
                server_status_checked
                    .borrow_mut()
                    .insert(remote.name.clone(), true);
                set_server_status(&server_rows, &remote.name, ServerStatus::Working);
            }
        }

        // Notify that we've finished checking all remotes for changes.
        let error_count = sync_errors_count();

        if error_count != 0 {
            let error_msg = if error_count == 1 {
                "Finished sync checks with 1 error.".to_string()
            } else {
                tr::tr!("Finished sync checks with {} errors.", error_count)
            };
            handle.update(|tray| tray.set_msg(error_msg));
        } else {
            handle.update(|tray| {
                tray.set_msg(tr::tr!("Finished sync checks."));
                tray.set_done();
            });
        }
    }

    // We broke out of the loop because of a close request, so stop the tray app,
    // and then close and destroy the window.
    window.close();
    window.destroy();
}
