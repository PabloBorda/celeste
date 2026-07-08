//! Functions and libcelesteities for logging in to a server.
use crate::{
    entities::{RemotesActiveModel, RemotesColumn, RemotesEntity, RemotesModel},
    gtk_util,
    mpsc::{self, Sender},
    rclone,
    traits::prelude::*,
    util,
};
mod dropbox;
mod gdrive;
pub mod login_util;
mod nextcloud;
mod owncloud;
mod pcloud;
mod proton_drive;
mod webdav;

use adw::{
    glib,
    gtk::{
        Align, Box, Button, FileChooserAction, FileChooserDialog, FileFilter, Image, Inhibit, Label,
        ListBox, Orientation, ResponseType, SelectionMode, SignalListItemFactory, StringList,
        StringObject,
    },
    prelude::*,
    Application, ApplicationWindow, ComboRow, EntryRow, HeaderBar,
};
use dropbox::DropboxConfig;
use gdrive::GDriveConfig;
use nextcloud::NextcloudConfig;
use owncloud::OwncloudConfig;
use pcloud::PCloudConfig;
use proton_drive::ProtonDriveConfig;
use std::{cell::RefCell, fs, path::Path, rc::Rc, time::Instant};
use webdav::WebDavConfig;

use sea_orm::{entity::prelude::*, ActiveValue, DatabaseConnection};
use serde_json::json;

static CLOUD_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#99c1f1" d="M8 18h10a5 5 0 0 0 .6-10A7 7 0 0 0 5.1 6.1 5.5 5.5 0 0 0 8 18z"/></svg>"##;
static WEBDAV_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#8ff0a4" d="M3 5h18v14H3z"/><path fill="#1c71d8" d="M5 7h14v3H5z"/><path fill="#fff" d="M6 13h12v2H6z"/></svg>"##;
static LOCKED_CLOUD_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#99c1f1" d="M8 18h10a5 5 0 0 0 .6-10A7 7 0 0 0 5.1 6.1 5.5 5.5 0 0 0 8 18z"/><path fill="#f9f06b" d="M9 12h8v7H9z"/><path fill="#5e5c64" d="M11 12v-1a2 2 0 1 1 4 0v1h-1.5v-1a.5.5 0 0 0-1 0v1z"/></svg>"##;
static RCLONE_ICON: &[u8] = include_bytes!("../../assets/rclone-import.svg");

thread_local! {
    static REMOTE_IMPORT_QUEUE: RefCell<Vec<RemotesModel>> = RefCell::new(vec![]);
}

fn server_type_icon_bytes(server_type: &str) -> &'static [u8] {
    match server_type.to_lowercase().as_str() {
        "google drive" => include_bytes!("../images/google-drive.png").as_slice(),
        "nextcloud" | "owncloud" | "webdav" => WEBDAV_ICON,
        "proton drive" => LOCKED_CLOUD_ICON,
        _ => CLOUD_ICON,
    }
}

fn server_type_factory() -> SignalListItemFactory {
    let factory = SignalListItemFactory::new();

    factory.connect_setup(|_, list_item| {
        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .margin_top(4)
            .margin_end(4)
            .margin_bottom(4)
            .margin_start(4)
            .build();
        row.append(&gtk_util::image_from_bytes(CLOUD_ICON, 22, 22));
        row.append(
            &Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .hexpand_set(true)
                .build(),
        );
        list_item.set_child(Some(&row));
    });

    factory.connect_bind(|_, list_item| {
        let Some(server_type) = list_item
            .item()
            .and_downcast::<StringObject>()
            .map(|item| item.string().to_string())
        else {
            return;
        };
        let Some(row) = list_item.child().and_downcast::<Box>() else {
            return;
        };
        let Some(icon) = row.first_child().and_downcast::<Image>() else {
            return;
        };
        let Some(label) = icon.next_sibling().and_downcast::<Label>() else {
            return;
        };

        gtk_util::set_image_from_bytes(&icon, server_type_icon_bytes(&server_type), 22, 22);
        label.set_label(&server_type);
    });

    factory
}

fn parse_rclone_remote_names(config: &str) -> Vec<String> {
    config
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                Some(trimmed[1..trimmed.len() - 1].trim().to_string())
            } else {
                None
            }
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn merge_rclone_config(existing_config: &str, imported_config: &str) -> String {
    let imported_names = parse_rclone_remote_names(imported_config);
    let mut output = String::new();
    let mut skip_existing = false;

    for line in existing_config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section_name = trimmed[1..trimmed.len() - 1].trim();
            skip_existing = imported_names.iter().any(|name| name == section_name);
        }

        if !skip_existing {
            output.push_str(line);
            output.push('\n');
        }
    }

    if !output.trim().is_empty() {
        output.push('\n');
    }
    output.push_str(imported_config.trim());
    output.push('\n');
    output
}

fn import_rclone_config(path: &Path, db: &DatabaseConnection) -> Result<Vec<RemotesModel>, String> {
    let started = Instant::now();
    util::log_line(&format!("rclone config import start path={}", path.display()));
    let imported_config = fs::read_to_string(path)
        .map_err(|err| tr::tr!("Unable to read rclone config: {}", err))?;
    let remote_names = parse_rclone_remote_names(&imported_config);

    if remote_names.is_empty() {
        return Err(tr::tr!("The selected file does not contain any rclone remotes."));
    }

    let mut celeste_config_path = util::get_config_dir();
    fs::create_dir_all(&celeste_config_path)
        .map_err(|err| tr::tr!("Unable to create Celeste config directory: {}", err))?;
    celeste_config_path.push("rclone.conf");

    let existing_config = fs::read_to_string(&celeste_config_path).unwrap_or_default();
    fs::write(
        &celeste_config_path,
        merge_rclone_config(&existing_config, &imported_config),
    )
    .map_err(|err| tr::tr!("Unable to write Celeste rclone config: {}", err))?;

    librclone::rpc(
        "config/setpath",
        json!({ "path": celeste_config_path }).to_string(),
    )
    .map_err(|err| tr::tr!("Unable to reload rclone config: {}", err))?;

    let mut imported_remotes = vec![];
    for remote_name in remote_names {
        let existing = util::await_future(
            RemotesEntity::find()
                .filter(RemotesColumn::Name.eq(remote_name.clone()))
                .one(db),
        )
        .map_err(|err| tr::tr!("Unable to inspect existing remotes: {}", err))?;

        if let Some(existing) = existing {
            imported_remotes.push(existing);
            continue;
        }

        let model = util::await_future(
            RemotesActiveModel {
                name: ActiveValue::Set(remote_name),
                ..Default::default()
            }
            .insert(db),
        )
        .map_err(|err| tr::tr!("Unable to add imported remote: {}", err))?;
        imported_remotes.push(model);
    }

    util::log_line(&format!(
        "rclone config import finished path={} remotes={} elapsed_ms={}",
        path.display(),
        imported_remotes.len(),
        started.elapsed().as_millis()
    ));
    Ok(imported_remotes)
}

/// A trait to get some data from configs.
trait LoginTrait {
    fn get_sections(
        window: &ApplicationWindow,
        sender: Sender<Option<ServerType>>,
    ) -> (Vec<EntryRow>, Button);
}

/// An enum representing valid storage types.
#[derive(Clone, Debug)]
pub enum ServerType {
    Dropbox(dropbox::DropboxConfig),
    GDrive(gdrive::GDriveConfig),
    Nextcloud(nextcloud::NextcloudConfig),
    Owncloud(owncloud::OwncloudConfig),
    PCloud(pcloud::PCloudConfig),
    ProtonDrive(proton_drive::ProtonDriveConfig),
    WebDav(webdav::WebDavConfig),
}

impl ToString for ServerType {
    fn to_string(&self) -> String {
        match self {
            Self::Dropbox(_) => "Dropbox",
            Self::GDrive(_) => "Google Drive",
            Self::Nextcloud(_) => "Nextcloud",
            Self::Owncloud(_) => "Owncloud",
            Self::PCloud(_) => "pCloud",
            Self::ProtonDrive(_) => "Proton Drive",
            Self::WebDav(_) => "WebDAV",
        }
        .to_string()
    }
}

// Verify if a specific config can log in to a server.
pub fn can_login(_app: &Application, config_name: &str) -> bool {
    if let Err(err) = rclone::sync::stat(config_name, "/") {
        let err_msg = if err.error.contains("Temporary failure in name resolution") {
            tr::tr!(
                "Unable to connect to the server. Check your internet connection and try again."
            )
        } else if err.error.contains("this account requires a 2FA code") {
            tr::tr!("A 2FA code is required to log in to this account. Provide one and try again.")
        } else {
            tr::tr!(
                "Unable to authenticate to the server. Check your login credentials and try again."
            )
        };

        gtk_util::show_error(&tr::tr!("Unable to log in"), Some(&err_msg));
        false
    } else {
        true
    }
}

/// Create a new session. Returns [`Some`] with the new session if the client
/// successfully logged in, and [`None`] on other events, such as closing the
/// window before logging in. Logged in clients can be obtained after this point
/// via [`rclone::get_configs`].
pub fn login(app: &Application, db: &DatabaseConnection) -> Option<Vec<RemotesModel>> {
    // The mspc sender/receiver to get data from fields.
    let (sender, mut receiver) = mpsc::channel::<Option<ServerType>>();

    // The window.
    let window = ApplicationWindow::builder()
        .application(app)
        .title(&util::get_title!("Log in"))
        .width_request(400)
        .build();
    window.add_css_class("celeste-global-padding");
    window.connect_close_request(glib::clone!(@strong sender => move |_| {
        sender.send(None);
        Inhibit(false)
    }));

    // The stack containing the forms for all login sections.
    let dropbox_name = ServerType::Dropbox(Default::default()).to_string();
    let gdrive_name = ServerType::GDrive(Default::default()).to_string();
    let nextcloud_name = ServerType::Nextcloud(Default::default()).to_string();
    let owncloud_name = ServerType::Owncloud(Default::default()).to_string();
    let pcloud_name = ServerType::PCloud(Default::default()).to_string();
    let proton_drive_name = ServerType::ProtonDrive(Default::default()).to_string();
    let webdav_name = ServerType::WebDav(Default::default()).to_string();

    // The dropdown for selecting the server type.
    let selected_factory = server_type_factory();
    let list_factory = server_type_factory();
    let server_type_dropdown = ComboRow::builder()
        .title(&tr::tr!("Server Type"))
        .factory(&selected_factory)
        .list_factory(&list_factory)
        .build();
    let server_types_array = [
        dropbox_name.as_str(),
        gdrive_name.as_str(),
        nextcloud_name.as_str(),
        owncloud_name.as_str(),
        pcloud_name.as_str(),
        proton_drive_name.as_str(),
        webdav_name.as_str(),
    ];
    let server_types = StringList::new(&server_types_array);
    server_type_dropdown.set_model(Some(&server_types));
    let server_type_icon = gtk_util::image_from_bytes(server_type_icon_bytes(dropbox_name.as_str()), 22, 22);
    server_type_dropdown.add_prefix(&server_type_icon);

    // A box containing the header bar and input sections.
    let container = Box::builder().orientation(Orientation::Vertical).build();
    let input_sections = ListBox::builder()
        .selection_mode(SelectionMode::None)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    container.append(&HeaderBar::new());
    container.append(&input_sections);
    input_sections.append(&server_type_dropdown);

    // Set up the submit button.
    let submit_button = login_util::submit_button();
    let import_button_content = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();
    import_button_content.append(&gtk_util::image_from_bytes(RCLONE_ICON, 22, 22));
    import_button_content.append(&Label::builder().label(&tr::tr!("Import rclone.conf")).build());
    let import_button = Button::builder()
        .child(&import_button_content)
        .halign(Align::End)
        .margin_top(10)
        .build();
    import_button.connect_clicked(glib::clone!(@strong db, @strong sender, @weak window => move |_| {
        window.set_sensitive(false);
        let filter = FileFilter::new();
        filter.set_name(Some(&tr::tr!("rclone config files")));
        filter.add_pattern("*.conf");

        let dialog = FileChooserDialog::builder()
            .title(&util::get_title!("Import rclone.conf"))
            .action(FileChooserAction::Open)
            .select_multiple(false)
            .filter(&filter)
            .transient_for(&window)
            .modal(true)
            .build();
        let cancel_button = Button::with_label(&tr::tr!("Cancel"));
        let import_dialog_button = Button::with_label(&tr::tr!("Import"));
        dialog.add_action_widget(&cancel_button, ResponseType::Cancel);
        dialog.add_action_widget(&import_dialog_button, ResponseType::Accept);
        dialog.connect_response(glib::clone!(@strong db, @strong sender, @weak window => move |dialog, resp| {
            if resp == ResponseType::Accept {
                if let Some(path) = dialog.file().and_then(|file| file.path()) {
                    match import_rclone_config(&path, &db) {
                        Ok(remotes) => {
                            REMOTE_IMPORT_QUEUE.with(|queue| {
                                *queue.borrow_mut() = remotes;
                            });
                            sender.send(None);
                            dialog.close();
                            window.close();
                            return;
                        }
                        Err(err) => {
                            gtk_util::show_error(&tr::tr!("Unable to import rclone.conf"), Some(&err));
                        }
                    }
                }
            }

            dialog.close();
            window.set_sensitive(true);
        }));
        dialog.show();
    }));
    container.append(&submit_button);
    container.append(&import_button);

    // Get the window items for each server type.
    let dropbox_items = DropboxConfig::get_sections(&window, sender.clone());
    let gdrive_items = GDriveConfig::get_sections(&window, sender.clone());
    let nextcloud_items = NextcloudConfig::get_sections(&window, sender.clone());
    let owncloud_items = OwncloudConfig::get_sections(&window, sender.clone());
    let pcloud_items = PCloudConfig::get_sections(&window, sender.clone());
    let proton_drive_items = ProtonDriveConfig::get_sections(&window, sender.clone());
    let webdav_items = WebDavConfig::get_sections(&window, sender);

    // Store the active items.
    let active_items: Rc<RefCell<(Vec<EntryRow>, Button)>> =
        Rc::new(RefCell::new((vec![], submit_button)));

    // Configure the window to change the widgets when the selected server type
    // changes.
    server_type_dropdown.connect_selected_notify(glib::clone!(@weak container, @weak input_sections, @weak server_type_icon, @strong server_types, @strong nextcloud_items, @strong webdav_items, @strong active_items => move |server_type_dropdown| {
        let server_type = server_types.string(server_type_dropdown.selected()).unwrap().to_string();
        gtk_util::set_image_from_bytes(&server_type_icon, server_type_icon_bytes(&server_type), 22, 22);

        let (rows, submit_button) = match server_type.to_lowercase().as_str() {
            "dropbox" => dropbox_items.clone(),
            "google drive" => gdrive_items.clone(),
            "nextcloud" => nextcloud_items.clone(),
            "owncloud" => owncloud_items.clone(),
            "pcloud" => pcloud_items.clone(),
            "proton drive" => proton_drive_items.clone(),
            "webdav" => webdav_items.clone(),
            _ => unreachable!()
        };

        // Remove the current submit button.
        let mut ptr = active_items.get_mut_ref();
        container.remove(&ptr.1);

        // Now remove the current listbox items.
        for row in ptr.0.clone()  {
            // Reset the row to default styling and text so that when the user goes back it looks like a fresh page.
            row.set_text("");
            row.remove_css_class("error");

            // Actually remove the item.
            input_sections.remove(&row);
            ptr.0.remove(0);
        }

        // Now set the ones for this remove.
        for row in rows {
            input_sections.append(&row);
            ptr.0.push(row);
        }

        // Now set the new submit button.
        container.append(&submit_button);
        ptr.1 = submit_button;
    }));
    // Go back and forth to the first widget so we can initialize our entries.
    server_type_dropdown.set_selected(1);
    server_type_dropdown.set_selected(0);

    // Set up the window and show it.
    window.set_content(Some(&container));
    window.show();

    // Keep receiving values from the windows on the stack until a valid config
    // is found.
    loop {
        // If the user clicks the 'X' button on the window we get a [`None`] value.
        let server = match receiver.recv() {
            Some(server) => server,
            None => {
                let imported_remotes = REMOTE_IMPORT_QUEUE.with(|queue| {
                    let mut queue = queue.borrow_mut();
                    let remotes = queue.clone();
                    queue.clear();
                    remotes
                });

                if imported_remotes.is_empty() {
                    return None;
                } else {
                    return Some(imported_remotes);
                }
            }
        };
        window.set_sensitive(false);

        // Create a new config with the requested name.
        let config_name = match &server {
            ServerType::Dropbox(config) => config.server_name.clone(),
            ServerType::GDrive(config) => config.server_name.clone(),
            ServerType::Nextcloud(config) => config.server_name.clone(),
            ServerType::Owncloud(config) => config.server_name.clone(),
            ServerType::PCloud(config) => config.server_name.clone(),
            ServerType::ProtonDrive(config) => config.server_name.clone(),
            ServerType::WebDav(config) => config.server_name.clone(),
        };

        let config_query = match &server {
            ServerType::Dropbox(config) => json!({
                "name": config_name,
                "parameters": {
                    "client_id": config.client_id,
                    "client_secret": config.client_secret,
                    "token": config.auth_json,
                    "config_refresh_token": false
                },
                "type": "dropbox"
            }),
            ServerType::GDrive(config) => json!({
                "name": config_name,
                "parameters": {
                    "client_id": config.client_id,
                    "client_secret": config.client_secret,
                    "token": config.auth_json,
                    "config_refresh_token": false
                },
                "type": "drive"
            }),
            ServerType::Nextcloud(config) => json!({
                "name": config_name,
                "parameters": {
                    "url": config.server_url,
                    "vendor": "nextcloud",
                    "user": config.username,
                    "pass": config.password
                },
                "type": "webdav",
                "opt": {
                    "obscure": true
                }
            }),
            ServerType::Owncloud(config) => json!({
                "name": config_name,
                "parameters": {
                    "url": config.server_url,
                    "vendor": "owncloud",
                    "user": config.username,
                    "pass": config.password
                },
                "type": "webdav",
                "opt": {
                    "obscure": true
                }
            }),
            ServerType::PCloud(config) => json!({
                "name": config_name,
                "parameters": {
                    "client_id": config.client_id,
                    "client_secret": config.client_secret,
                    "token": config.auth_json,
                    "config_refresh_token": false
                },
                "type": "pcloud",
                "opt": {
                    "obscure": true
                }
            }),
            ServerType::ProtonDrive(config) => json!({
                "name": config_name,
                "parameters": {
                    "username": config.username,
                    "password": config.password,
                    "2fa": config.totp
                },
                "type": "protondrive",
                "opt": {
                    "obscure": true
                }
            }),
            ServerType::WebDav(config) => json!({
                "name": config_name,
                "parameters": {
                    "url": config.server_url,
                    "vendor": "webdav",
                    "user": config.username,
                    "pass": config.password
                },
                "type": "webdav",
                "opt": {
                    "obscure": true
                }
            }),
        };

        util::run_in_background(move || {
            librclone::rpc("config/create", config_query.to_string()).unwrap()
        });

        // If we can't connect to the server, assume invalid credentials were given,
        // remote the config, and try asking for input again.
        if !can_login(app, &config_name) {
            util::run_in_background(move || {
                librclone::rpc("config/delete", json!({ "name": config_name }).to_string()).unwrap()
            });
            window.set_sensitive(true);
        // We've passed validation otherwise, so add the remote to the db, close
        // the window and return the config.
        } else {
            let model = util::await_future(
                RemotesActiveModel {
                    name: ActiveValue::Set(config_name),
                    ..Default::default()
                }
                .insert(db),
            )
            .unwrap();

            window.close();
            return Some(vec![model]);
        }
    }
}
