use adw::gtk::gdk_pixbuf::PixbufLoader;
use crate::{launch, util};
use adw::prelude::PixbufLoaderExt;
use ksni::{menu::StandardItem, Icon, MenuItem, Tray as KsniTray};

fn tray_icon_bytes(icon_name: &str) -> &'static [u8] {
    match icon_name {
        "com.hunterwittenborn.Celeste.CelesteTrayLoading-symbolic" => include_bytes!(
            "../assets/context/com.hunterwittenborn.Celeste.CelesteTrayLoading-symbolic.svg"
        ),
        "com.hunterwittenborn.Celeste.CelesteTraySyncing-symbolic" => include_bytes!(
            "../assets/context/com.hunterwittenborn.Celeste.CelesteTraySyncing-symbolic.svg"
        ),
        "com.hunterwittenborn.Celeste.CelesteTrayWarning-symbolic" => include_bytes!(
            "../assets/context/com.hunterwittenborn.Celeste.CelesteTrayWarning-symbolic.svg"
        ),
        "com.hunterwittenborn.Celeste.CelesteTrayDone-symbolic" => include_bytes!(
            "../assets/context/com.hunterwittenborn.Celeste.CelesteTrayDone-symbolic.svg"
        ),
        "com.hunterwittenborn.Celeste.CelesteTrayDisconnected-symbolic" => include_bytes!(
            "../assets/context/com.hunterwittenborn.Celeste.CelesteTrayDisconnected-symbolic.svg"
        ),
        _ => include_bytes!("../assets/context/com.hunterwittenborn.Celeste.CelesteTrayLoading-symbolic.svg"),
    }
}

fn tray_pixmap(bytes: &'static [u8], size: i32) -> Option<Icon> {
    let loader = PixbufLoader::new();
    loader.set_size(size, size);

    if !loader.write(bytes).is_ok() || !loader.close().is_ok() {
        return None;
    }

    let pixbuf = loader.pixbuf()?;
    let pixel_bytes = pixbuf.read_pixel_bytes()?;
    let pixels = pixel_bytes.as_ref();
    let rowstride = pixbuf.rowstride() as usize;
    let channels = pixbuf.n_channels() as usize;
    let width = pixbuf.width() as usize;
    let height = pixbuf.height() as usize;
    let mut data = Vec::with_capacity(width * height * 4);

    for y in 0..height {
        for x in 0..width {
            let offset = y * rowstride + x * channels;
            let red = pixels[offset];
            let green = pixels[offset + 1];
            let blue = pixels[offset + 2];
            let alpha = if channels >= 4 { pixels[offset + 3] } else { 255 };
            data.extend_from_slice(&[alpha, red, green, blue]);
        }
    }

    Some(Icon {
        width: pixbuf.width(),
        height: pixbuf.height(),
        data,
    })
}

pub struct Tray {
    status: String,
    pub icon: String,
}

impl Tray {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            status: tr::tr!("Awaiting sync checks..."),
            icon: "com.hunterwittenborn.Celeste.CelesteTrayLoading-symbolic".to_owned(),
        }
    }

    pub fn set_msg<T: ToString>(&mut self, msg: T) {
        self.status = msg.to_string();
    }

    pub fn set_syncing(&mut self) {
        self.icon = "com.hunterwittenborn.Celeste.CelesteTraySyncing-symbolic".to_owned();
    }

    pub fn set_warning(&mut self) {
        self.icon = "com.hunterwittenborn.Celeste.CelesteTrayWarning-symbolic".to_owned();
    }

    pub fn set_done(&mut self) {
        self.icon = "com.hunterwittenborn.Celeste.CelesteTrayDone-symbolic".to_owned();
    }

    pub fn set_disconnected(&mut self) {
        self.icon = "com.hunterwittenborn.Celeste.CelesteTrayDisconnected-symbolic".to_owned();
    }
}

impl KsniTray for Tray {
    fn icon_name(&self) -> String {
        self.icon.clone()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let bytes = tray_icon_bytes(&self.icon);
        [16, 32]
            .into_iter()
            .filter_map(|size| tray_pixmap(bytes, size))
            .collect()
    }

    fn title(&self) -> String {
        "Celeste".to_owned()
    }

    fn id(&self) -> String {
        util::APP_ID.to_owned()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            MenuItem::Standard(StandardItem {
                label: self.status.clone(),
                enabled: false,
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: tr::tr!("Open"),
                activate: Box::new(|_| {
                    *(*launch::OPEN_REQUEST).lock().unwrap() = true;
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: tr::tr!("Close"),
                activate: Box::new(|_| {
                    *(*launch::CLOSE_REQUEST).lock().unwrap() = true;
                }),
                ..Default::default()
            }),
        ]
    }
}
