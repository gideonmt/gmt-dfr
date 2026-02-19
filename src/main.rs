use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface, Surface};
use chrono::{Local, Locale, Timelike, format::{StrftimeItems, Item as ChronoItem}};
use drm::control::ClipRect;
use freedesktop_icons::lookup;
use input::{
    event::{
        device::DeviceEvent,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot},
        Event, EventTrait,
    },
    Device as InputDevice, Libinput, LibinputInterface,
};
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{input_event, input_id, timeval, uinput_setup};
use libc::{c_char, O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};
use nix::{
    errno::Errno,
    sys::{
        epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
        signal::{SigSet, Signal},
    },
};
use privdrop::PrivDrop;
use std::{
    cmp::min,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    os::{
        fd::{AsFd, AsRawFd},
        unix::{fs::OpenOptionsExt, io::OwnedFd},
    },
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
};
use udev::MonitorBuilder;

mod backlight;
mod config;
mod display;
mod fonts;
mod niri;
mod pixel_shift;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use config::{ButtonConfig, Config};
use display::DrmBackend;
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};

const BUTTON_SPACING_PX: i32 = 16;
const ICON_SIZE: i32 = 48;
const TIMEOUT_MS: i32 = 10 * 1000;
const FN_TAP_THRESHOLD_MS: u128 = 300;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BatteryState {
    NotCharging,
    Charging,
    Low,
}

struct BatteryImages {
    plain: Vec<Handle>,
    charging: Vec<Handle>,
    bolt: Handle,
}

#[derive(Eq, PartialEq, Copy, Clone)]
enum BatteryIconMode {
    Percentage,
    Icon,
    Both,
}

impl BatteryIconMode {
    fn should_draw_icon(self) -> bool {
        self != BatteryIconMode::Percentage
    }
    fn should_draw_text(self) -> bool {
        self != BatteryIconMode::Icon
    }
}

fn get_volume_percent() -> Option<(u32, bool)> {
    Some((0, true))
}

fn get_brightness_percent() -> Option<u32> {
    None
}

#[derive(Clone, Debug)]
pub struct WifiInfo {
    pub ssid: String,
    pub signal: i32,
}

fn get_wifi_info() -> Option<WifiInfo> {
    None
}

enum ButtonImage {
    Text(String),
    Svg(Handle),
    Bitmap(ImageSurface),
    Time(Vec<ChronoItem<'static>>, Locale),
    Battery(String, BatteryIconMode, BatteryImages),
    Volume,
    Brightness,
    Wifi,
    NiriWorkspace { idx: u8, focused: bool },
    NiriWindowTitle(String),
    Spacer,
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: Vec<Key>,
    clickable: bool,
}

fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(
        Handle::from_file(path)?.ok_or(anyhow!("failed to load image"))?,
    ))
}

fn try_load_png(path: impl AsRef<Path>) -> Result<ButtonImage> {
    let mut file = File::open(path)?;
    let surf = ImageSurface::create_from_png(&mut file)?;
    if surf.height() == ICON_SIZE && surf.width() == ICON_SIZE {
        return Ok(ButtonImage::Bitmap(surf));
    }
    let resized = ImageSurface::create(Format::ARgb32, ICON_SIZE, ICON_SIZE).unwrap();
    let c = Context::new(&resized).unwrap();
    c.scale(
        ICON_SIZE as f64 / surf.width() as f64,
        ICON_SIZE as f64 / surf.height() as f64,
    );
    c.set_source_surface(surf, 0.0, 0.0).unwrap();
    c.set_antialias(Antialias::Best);
    c.paint().unwrap();
    Ok(ButtonImage::Bitmap(resized))
}

fn try_load_image(name: impl AsRef<str>, theme: Option<impl AsRef<str>>) -> Result<ButtonImage> {
    let name = name.as_ref();
    let locations;

    if let Some(theme) = theme {
        let theme = theme.as_ref();
        let candidates = vec![
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .with_size(ICON_SIZE as u16)
                .force_svg()
                .find(),
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .force_svg()
                .find(),
        ];
        locations = candidates.into_iter().flatten().collect();
    } else {
        locations = vec![
            PathBuf::from(format!("/etc/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/etc/tiny-dfr/{name}.png")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.png")),
        ];
    };

    let mut last_err = anyhow!("no suitable icon path was found");

    for location in locations {
        let result = match location.extension().and_then(|s| s.to_str()) {
            Some("png") => try_load_png(&location),
            Some("svg") => try_load_svg(
                location
                    .to_str()
                    .ok_or(anyhow!("image path is not unicode"))?,
            ),
            _ => Err(anyhow!("invalid file extension")),
        };

        match result {
            Ok(image) => return Ok(image),
            Err(err) => {
                last_err = err.context(format!("while loading path {}", location.display()));
            }
        };
    }

    Err(last_err.context(format!(
        "failed loading all possible paths for icon {name}"
    )))
}

fn find_battery_device() -> Option<String> {
    let power_supply_path = "/sys/class/power_supply";
    if let Ok(entries) = fs::read_dir(power_supply_path) {
        for entry in entries.flatten() {
            let dev_path = entry.path();
            let type_path = dev_path.join("type");
            if let Ok(typ) = fs::read_to_string(&type_path) {
                if typ.trim() == "Battery" {
                    if let Some(name) = dev_path.file_name().and_then(|n| n.to_str()) {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

fn get_battery_state(battery: &str) -> (u32, BatteryState) {
    let status_path = format!("/sys/class/power_supply/{}/status", battery);
    let status = fs::read_to_string(&status_path).unwrap_or_else(|_| "Unknown".to_string());

    #[cfg(target_arch = "x86_64")]
    let capacity = {
        let charge_now_path = format!("/sys/class/power_supply/{}/charge_now", battery);
        let charge_full_path = format!("/sys/class/power_supply/{}/charge_full", battery);
        let charge_now = fs::read_to_string(&charge_now_path)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok());
        let charge_full = fs::read_to_string(&charge_full_path)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok());
        match (charge_now, charge_full) {
            (Some(now), Some(full)) if full > 0.0 => ((now / full) * 100.0).round() as u32,
            _ => 100,
        }
    };

    #[cfg(target_arch = "aarch64")]
    let capacity = {
        let capacity_path = format!("/sys/class/power_supply/{}/capacity", battery);
        fs::read_to_string(&capacity_path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(100)
    };

    let state = match status.trim() {
        "Charging" | "Full" => BatteryState::Charging,
        "Discharging" if capacity < 10 => BatteryState::Low,
        _ => BatteryState::NotCharging,
    };
    (capacity, state)
}

impl Button {
    fn with_config(cfg: ButtonConfig) -> Button {
        if let Some(text) = cfg.text {
            Button::new_text(text, cfg.action)
        } else if let Some(icon) = cfg.icon {
            Button::new_icon(&icon, cfg.theme, cfg.action)
        } else if let Some(time) = cfg.time {
            Button::new_time(cfg.action, &time, cfg.locale.as_deref())
        } else if let Some(battery_mode) = cfg.battery {
            if let Some(battery) = find_battery_device() {
                Button::new_battery(cfg.action, battery, battery_mode, cfg.theme)
            } else {
                Button::new_text("Battery N/A".to_string(), cfg.action)
            }
        } else if cfg.volume == Some(true) {
            Button::new_simple(ButtonImage::Volume, cfg.action, false)
        } else if cfg.brightness == Some(true) {
            Button::new_simple(ButtonImage::Brightness, cfg.action, false)
        } else if cfg.wifi == Some(true) {
            Button::new_simple(ButtonImage::Wifi, cfg.action, false)
        } else {
            Button::new_spacer()
        }
    }

    fn new_spacer() -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            clickable: true,
            image: ButtonImage::Spacer,
        }
    }

    fn new_text(text: String, action: Vec<Key>) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            clickable: true,
            image: ButtonImage::Text(text),
        }
    }

    fn new_simple(image: ButtonImage, action: Vec<Key>, clickable: bool) -> Button {
        Button {
            action,
            active: false,
            changed: true,
            clickable,
            image,
        }
    }

    fn new_icon(
        path: impl AsRef<str>,
        theme: Option<impl AsRef<str>>,
        action: Vec<Key>,
    ) -> Button {
        let image = try_load_image(path, theme).expect("failed to load icon");
        Button {
            action,
            image,
            active: false,
            changed: false,
            clickable: true,
        }
    }

    fn load_battery_image(icon: &str, theme: Option<impl AsRef<str>>) -> Handle {
        if let ButtonImage::Svg(svg) = try_load_image(icon, theme).unwrap() {
            return svg;
        }
        panic!("failed to load icon");
    }

    fn new_battery(
        action: Vec<Key>,
        battery: String,
        battery_mode: String,
        theme: Option<impl AsRef<str>>,
    ) -> Button {
        let bolt = Self::load_battery_image("bolt", theme.as_ref());
        let mut plain = Vec::new();
        let mut charging = Vec::new();
        for icon in [
            "battery_0_bar",
            "battery_1_bar",
            "battery_2_bar",
            "battery_3_bar",
            "battery_4_bar",
            "battery_5_bar",
            "battery_6_bar",
            "battery_full",
        ] {
            plain.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        for icon in [
            "battery_charging_20",
            "battery_charging_30",
            "battery_charging_50",
            "battery_charging_60",
            "battery_charging_80",
            "battery_charging_90",
            "battery_charging_full",
        ] {
            charging.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        let battery_mode = match battery_mode.as_str() {
            "icon" => BatteryIconMode::Icon,
            "percentage" => BatteryIconMode::Percentage,
            "both" => BatteryIconMode::Both,
            _ => panic!("invalid battery mode, accepted modes: icon, percentage, both"),
        };
        Button {
            action,
            active: false,
            changed: false,
            clickable: true,
            image: ButtonImage::Battery(
                battery,
                battery_mode,
                BatteryImages {
                    plain,
                    bolt,
                    charging,
                },
            ),
        }
    }

    fn new_time(action: Vec<Key>, format: &str, locale_str: Option<&str>) -> Button {
        let format_str = if format == "24hr" {
            "%H:%M    %a %-e %b"
        } else if format == "12hr" {
            "%-l:%M %p    %a %-e %b"
        } else {
            format
        };

        let format_items = match StrftimeItems::new(format_str).parse_to_owned() {
            Ok(s) => s,
            Err(e) => panic!("Invalid time format: {e:?}"),
        };

        let locale = locale_str
            .and_then(|l| Locale::try_from(l).ok())
            .unwrap_or(Locale::POSIX);
        Button {
            action,
            active: false,
            changed: false,
            clickable: false,
            image: ButtonImage::Time(format_items, locale),
        }
    }

    fn new_niri_workspace(idx: u8, focused: bool, id: u64) -> Button {
        let _ = id;
        Button {
            action: vec![],
            active: false,
            changed: true,
            clickable: true,
            image: ButtonImage::NiriWorkspace { idx, focused },
        }
    }

    fn new_niri_window_title(title: String) -> Button {
        Button {
            action: vec![],
            active: false,
            changed: true,
            clickable: false,
            image: ButtonImage::NiriWindowTitle(title),
        }
    }

    fn needs_faster_refresh(&self) -> bool {
        match &self.image {
            ButtonImage::Time(items, _) => items.iter().any(|item| {
                use chrono::format::{Item, Numeric};
                matches!(
                    item,
                    Item::Numeric(Numeric::Second, _)
                        | Item::Numeric(Numeric::Nanosecond, _)
                        | Item::Numeric(Numeric::Timestamp, _)
                )
            }),
            // Volume and brightness poll on every redraw cycle
            ButtonImage::Volume | ButtonImage::Brightness | ButtonImage::Wifi => false,
            _ => false,
        }
    }

    fn render(
        &self,
        c: &Context,
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
        cfg: &Config,
    ) {
        match &self.image {
            ButtonImage::Text(text) => {
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge
                        + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(text).unwrap();
            }
            ButtonImage::Svg(svg) => {
                let x = button_left_edge
                    + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();
                svg.render_document(c, &Rectangle::new(x, y, ICON_SIZE as f64, ICON_SIZE as f64))
                    .unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let x = button_left_edge
                    + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();
                c.set_source_surface(surf, x, y).unwrap();
                c.rectangle(x, y, ICON_SIZE as f64, ICON_SIZE as f64);
                c.fill().unwrap();
            }
            ButtonImage::Time(format, locale) => {
                let current_time = Local::now();
                let formatted_time = current_time
                    .format_localized_with_items(format.iter(), *locale)
                    .to_string();
                let time_extents = c.text_extents(&formatted_time).unwrap();
                c.move_to(
                    button_left_edge
                        + (button_width as f64 / 2.0 - time_extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + time_extents.height() / 2.0).round(),
                );
                c.show_text(&formatted_time).unwrap();
            }
            ButtonImage::Volume => {
                // Icons match waybar pulseaudio format-icons: 󰕿 󰖀 󰕾 and muted 󰝟
                let text = match get_volume_percent() {
                    Some((v, muted)) if muted => "\u{f075f}".to_string(),
                    Some((v, _)) => {
                        let icon = if v == 0 { "\u{f057f}" }
                                   else if v < 50 { "\u{f0580}" }
                                   else { "\u{f057e}" };
                        format!("{} {}%", icon, v)
                    }
                    None => "\u{f057e} --".to_string(),
                };
                render_centered_text(c, height, button_left_edge, button_width, y_shift, &text);
            }
            ButtonImage::Brightness => {
                // Icons match waybar backlight format-icons: 󱩎 through 󱩖 (9 steps)
                let text = match get_brightness_percent() {
                    Some(v) => {
                        let icons = ["\u{fe24e}", "\u{fe24f}", "\u{fe250}", "\u{fe251}",
                                     "\u{fe252}", "\u{fe253}", "\u{fe254}", "\u{fe255}", "\u{fe256}"];
                        let idx = ((v as usize).min(100) * (icons.len() - 1) / 100);
                        format!("{} {}%", icons[idx], v)
                    }
                    None => "\u{fe256} --".to_string(),
                };
                render_centered_text(c, height, button_left_edge, button_width, y_shift, &text);
            }
            ButtonImage::Wifi => {
                // Network icons: 󰤨 connected, 󰤭  disconnected
                let text = match get_wifi_info() {
                    Some(info) => {
                        let icon = wifi_icon(info.signal);
                        format!("{} {}", icon, truncate_ssid(&info.ssid, 8))
                    }
                    None => "\u{f0935}".to_string(),
                };
                render_centered_text(c, height, button_left_edge, button_width, y_shift, &text);
            }
            ButtonImage::NiriWorkspace { idx, .. } => {
                let label = idx.to_string();
                let extents = c.text_extents(&label).unwrap();
                c.move_to(
                    button_left_edge
                        + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(&label).unwrap();
            }
            ButtonImage::NiriWindowTitle(title) => {
                let max_w = button_width as f64 - 16.0;
                let full_extents = c.text_extents(title).unwrap();
                if full_extents.width() <= max_w {
                    let extents = c.text_extents(title).unwrap();
                    c.move_to(
                        button_left_edge
                            + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(title).unwrap();
                } else {
                    let ellipsis = "…";
                    let ellipsis_w = c.text_extents(ellipsis).unwrap().width();
                    let char_indices: Vec<_> = title.char_indices().collect();
                    let mut lo = 0usize;
                    let mut hi = char_indices.len();
                    while lo + 1 < hi {
                        let mid = (lo + hi) / 2;
                        let byte_end = char_indices[mid].0;
                        let candidate = &title[..byte_end];
                        let w = c.text_extents(candidate).unwrap().width();
                        if w + ellipsis_w <= max_w {
                            lo = mid;
                        } else {
                            hi = mid;
                        }
                    }
                    let byte_end = char_indices.get(lo).map(|(i, _)| *i).unwrap_or(0);
                    let truncated = format!("{}{}", &title[..byte_end], ellipsis);
                    let extents = c.text_extents(&truncated).unwrap();
                    c.move_to(
                        button_left_edge
                            + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&truncated).unwrap();
                }
            }
            ButtonImage::Battery(battery, battery_mode, icons) => {
                let (capacity, state) = get_battery_state(battery);
                let icon = if battery_mode.should_draw_icon() {
                    Some(match state {
                        BatteryState::Charging => match capacity {
                            0..=20 => &icons.charging[0],
                            21..=30 => &icons.charging[1],
                            31..=50 => &icons.charging[2],
                            51..=60 => &icons.charging[3],
                            61..=80 => &icons.charging[4],
                            81..=99 => &icons.charging[5],
                            _ => &icons.charging[6],
                        },
                        _ => match capacity {
                            0 => &icons.plain[0],
                            1..=20 => &icons.plain[1],
                            21..=30 => &icons.plain[2],
                            31..=50 => &icons.plain[3],
                            51..=60 => &icons.plain[4],
                            61..=80 => &icons.plain[5],
                            81..=99 => &icons.plain[6],
                            _ => &icons.plain[7],
                        },
                    })
                } else if state == BatteryState::Charging {
                    Some(&icons.bolt)
                } else {
                    None
                };
                let percent_str = format!("{:.0}%", capacity);
                let extents = c.text_extents(&percent_str).unwrap();
                let mut width = extents.width();
                let mut text_offset = 0;
                if let Some(svg) = icon {
                    if !battery_mode.should_draw_text() {
                        width = ICON_SIZE as f64;
                    } else {
                        width += ICON_SIZE as f64;
                    }
                    text_offset = ICON_SIZE;
                    let x =
                        button_left_edge + (button_width as f64 / 2.0 - width / 2.0).round();
                    let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();
                    svg.render_document(
                        c,
                        &Rectangle::new(x, y, ICON_SIZE as f64, ICON_SIZE as f64),
                    )
                    .unwrap();
                }
                if battery_mode.should_draw_text() {
                    c.move_to(
                        button_left_edge
                            + (button_width as f64 / 2.0 - width / 2.0
                                + text_offset as f64)
                                .round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&percent_str).unwrap();
                }
            }
            ButtonImage::Spacer => (),
        }
    }

    fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool)
    where
        F: AsRawFd,
    {
        if !self.clickable {
            return;
        }
        if self.active != active {
            self.active = active;
            self.changed = true;
            toggle_keys(uinput, &self.action, active as i32);
        }
    }

    fn set_background_color(&self, c: &Context, active: bool, theme: &crate::config::Theme) {
        let (r, g, b) = if active { theme.button_active } else { theme.button_inactive };
        match &self.image {
            ButtonImage::Battery(battery, _, _) => {
                let (_, state) = get_battery_state(battery);
                match state {
                    BatteryState::NotCharging => c.set_source_rgb(r, g, b),
                    BatteryState::Charging    => { let (r,g,b) = theme.success; c.set_source_rgb(r, g, b); }
                    BatteryState::Low         => { let (r,g,b) = theme.warning; c.set_source_rgb(r, g, b); }
                }
            }
            ButtonImage::NiriWorkspace { focused, .. } => {
                if *focused {
                    let (r,g,b) = theme.accent;
                    c.set_source_rgb(r, g, b);
                } else {
                    c.set_source_rgb(r, g, b);
                }
            }
            _ => c.set_source_rgb(r, g, b),
        }
    }
}

fn render_centered_text(
    c: &Context,
    height: i32,
    left: f64,
    width: u64,
    y_shift: f64,
    text: &str,
) {
    let extents = c.text_extents(text).unwrap();
    c.move_to(
        left + (width as f64 / 2.0 - extents.width() / 2.0).round(),
        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
    );
    c.show_text(text).unwrap();
}

// Nerd Font wifi icons by signal strength: 󰤯 󰤟 󰤢 󰤥 󰤨
fn wifi_icon(signal: i32) -> &'static str {
    match signal {
        80..=100 => "\u{f0928}",
        60..=79  => "\u{f0925}",
        40..=59  => "\u{f0922}",
        1..=39   => "\u{f091f}",
        _        => "\u{f092f}",
    }
}

fn truncate_ssid(ssid: &str, max_chars: usize) -> String {
    let chars: Vec<char> = ssid.chars().collect();
    if chars.len() <= max_chars {
        ssid.to_string()
    } else {
        let truncated: String = chars[..max_chars - 1].iter().collect();
        format!("{}…", truncated)
    }
}

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    displays_battery: bool,
    displays_live: bool,
    pub buttons: Vec<(usize, Button)>,
    pub virtual_button_count: usize,
    faster_refresh: bool,
    pub niri_workspace_ids: Vec<(usize, u8)>,
    pub source_config: Vec<ButtonConfig>,
}

impl FunctionLayer {
    fn with_config(cfg: Vec<ButtonConfig>) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }

        let mut virtual_button_count = 0;
        let displays_time = cfg.iter().any(|cfg| cfg.time.is_some());
        let displays_battery = cfg.iter().any(|cfg| cfg.battery.is_some());
        let displays_live = cfg.iter().any(|cfg| {
            cfg.volume == Some(true) || cfg.brightness == Some(true) || cfg.wifi == Some(true)
        });
        let buttons = cfg
            .into_iter()
            .scan(&mut virtual_button_count, |state, cfg| {
                let i = **state;
                let mut stretch = cfg.stretch.unwrap_or(1);
                if stretch < 1 {
                    println!("Stretch value must be at least 1, setting to 1.");
                    stretch = 1;
                }
                **state += stretch;
                Some((i, Button::with_config(cfg)))
            })
            .collect::<Vec<_>>();
        let faster_refresh = buttons.iter().any(|(_, b)| b.needs_faster_refresh());
        FunctionLayer {
            displays_time,
            displays_battery,
            displays_live,
            buttons,
            virtual_button_count,
            faster_refresh,
            niri_workspace_ids: vec![],
            source_config: vec![],
        }
    }

    fn draw(
        &mut self,
        config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
    ) -> Vec<ClipRect> {
        let c = Context::new(surface).unwrap();
        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            Vec::new()
        };
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let pixel_shift_width = if config.enable_pixel_shift {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let virtual_button_width = ((width - pixel_shift_width as i32)
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            as f64
            / self.virtual_button_count as f64;
        let radius = 8.0f64;
        let bot = (height as f64) * 0.15;
        let top = (height as f64) * 0.85;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            let (r,g,b) = config.theme.background;
            c.set_source_rgb(r, g, b);
            c.paint().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(config.font_size);

        for i in 0..self.buttons.len() {
            let end = if i + 1 < self.buttons.len() {
                self.buttons[i + 1].0
            } else {
                self.virtual_button_count
            };
            let (start, button) = &mut self.buttons[i];
            let start = *start;

            if !button.changed && !complete_redraw {
                continue;
            };

            let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor()
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();

            if !complete_redraw {
                let (r,g,b) = config.theme.background;
                c.set_source_rgb(r, g, b);
                c.rectangle(
                    left_edge,
                    bot - radius,
                    button_width,
                    top - bot + radius * 2.0,
                );
                c.fill().unwrap();
            }

            let draw_active = button.active;
            let draw_outline = config.show_button_outlines || button.active;
            if !matches!(button.image, ButtonImage::Spacer) && button.clickable && draw_outline {
                button.set_background_color(&c, draw_active, &config.theme);
                c.new_sub_path();
                let left = left_edge + radius;
                let right = (left_edge + button_width.ceil()) - radius;
                c.arc(right, bot, radius, (-90.0f64).to_radians(), (0.0f64).to_radians());
                c.arc(right, top, radius, (0.0f64).to_radians(), (90.0f64).to_radians());
                c.arc(left, top, radius, (90.0f64).to_radians(), (180.0f64).to_radians());
                c.arc(left, bot, radius, (180.0f64).to_radians(), (270.0f64).to_radians());
                c.close_path();
                c.fill().unwrap();
            }

            let (r,g,b) = config.theme.foreground;
            c.set_source_rgb(r, g, b);
            button.render(&c, height, left_edge, button_width.ceil() as u64, pixel_shift_y, config);

            button.changed = false;

            if !complete_redraw {
                modified_regions.push(ClipRect::new(
                    height as u16 - top as u16 - radius as u16,
                    left_edge as u16,
                    height as u16 - bot as u16 + radius as u16,
                    left_edge as u16 + button_width as u16,
                ));
            }
        }

        modified_regions
    }

    fn hit(&self, width: u16, height: u16, x: f64, y: f64, i: Option<usize>) -> Option<usize> {
        let virtual_button_width =
            (width as i32 - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32)) as f64
                / self.virtual_button_count as f64;

        let i = i.unwrap_or_else(|| {
            let virtual_i = (x / (width as f64 / self.virtual_button_count as f64)) as usize;
            self.buttons
                .iter()
                .position(|(start, _)| *start > virtual_i)
                .unwrap_or(self.buttons.len())
                - 1
        });
        if i >= self.buttons.len() {
            return None;
        }

        if !self.buttons[i].1.clickable {
            return None;
        }

        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };

        let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor();
        let button_width = virtual_button_width
            + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor();

        if x < left_edge
            || x > (left_edge + button_width)
            || y < 0.1 * height as f64
            || y > 0.9 * height as f64
        {
            return None;
        }

        Some(i)
    }
}

fn rebuild_info_layer(layers: &mut Vec<FunctionLayer>, niri_state: &niri::NiriState) {
    let Some(info_cfg) = layers.get(1).map(|l| l.source_config.clone()) else {
        return;
    };
    let Some(layer) = layers.get_mut(1) else { return };

    let mut buttons: Vec<(usize, Button)> = Vec::new();
    let mut niri_workspace_ids: Vec<(usize, u8)> = Vec::new();
    let mut virt = 0usize;
    let mut total = 0usize;
    let mut displays_time = false;
    let mut faster_refresh = false;
    let mut displays_live = false;

    for cfg in &info_cfg {
        let stretch = cfg.stretch.unwrap_or(1);

        if cfg.niri_workspaces == Some(true) {
            for ws in &niri_state.workspaces {
                let btn_index = buttons.len();
                niri_workspace_ids.push((btn_index, ws.idx));
                buttons.push((virt, Button::new_niri_workspace(ws.idx, ws.is_focused, ws.id)));
                virt += 1;
                total += 1;
            }
            continue;
        }

        if cfg.niri_window_title == Some(true) {
            let title = niri_state.focused_window_title.clone().unwrap_or_default();
            buttons.push((virt, Button::new_niri_window_title(title)));
            virt += stretch;
            total += stretch;
            continue;
        }

        let btn = Button::with_config(cfg.clone());
        if matches!(btn.image, ButtonImage::Time(..)) {
            displays_time = true;
            faster_refresh = btn.needs_faster_refresh();
        }
        if matches!(btn.image, ButtonImage::Volume | ButtonImage::Brightness | ButtonImage::Wifi) {
            displays_live = true;
        }
        buttons.push((virt, btn));
        virt += stretch;
        total += stretch;
    }

    layer.buttons = buttons;
    layer.virtual_button_count = total.max(virt);
    layer.niri_workspace_ids = niri_workspace_ids;
    layer.displays_time = displays_time;
    layer.faster_refresh = faster_refresh;
    layer.displays_live = displays_live;
}

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;
        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32)
where
    F: AsRawFd,
{
    uinput
        .write(&[input_event {
            value,
            type_: ty as u16,
            code,
            time: timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        }])
        .unwrap();
}

fn toggle_keys<F>(uinput: &mut UInputHandle<F>, codes: &Vec<Key>, value: i32)
where
    F: AsRawFd,
{
    if codes.is_empty() {
        return;
    }
    for kc in codes {
        emit(uinput, EventKind::Key, *kc as u16, value);
    }
    emit(
        uinput,
        EventKind::Synchronize,
        SynchronizeKind::Report as u16,
        0,
    );
}

fn main() {
    let mut drm = DrmBackend::open_card().unwrap();
    let (height, width) = drm.mode().size();
    let _ = panic::catch_unwind(AssertUnwindSafe(|| real_main(&mut drm)));
    let crash_bitmap = include_bytes!("crash_bitmap.raw");
    let mut map = drm.map().unwrap();
    let data = map.as_mut();
    let mut wptr = 0;
    for byte in crash_bitmap {
        for i in 0..8 {
            let bit = ((byte >> i) & 0x1) == 0;
            let color = if bit { 0xFF } else { 0x0 };
            data[wptr] = color;
            data[wptr + 1] = color;
            data[wptr + 2] = color;
            data[wptr + 3] = color;
            wptr += 4;
        }
    }
    drop(map);
    drm.dirty(&[ClipRect::new(0, 0, height, width)]).unwrap();
    let mut sigset = SigSet::empty();
    sigset.add(Signal::SIGTERM);
    sigset.wait().unwrap();
}

fn real_main(drm: &mut DrmBackend) {
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm.fb_info().unwrap().size();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    let mut backlight = BacklightManager::new();
    let mut cfg_mgr = ConfigManager::new();
    let (mut cfg, mut layers) = cfg_mgr.load_config(width);
    let mut pixel_shift = PixelShiftManager::new();

    let mut niri: Option<niri::NiriState> = niri::NiriState::connect();
    if let Some(ref n) = niri {
        rebuild_info_layer(&mut layers, n);
    }

    let groups = ["input", "video"];
    PrivDrop::default()
        .user("nobody")
        .group_list(&groups)
        .apply()
        .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));

    let mut surface =
        ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut active_layer = 0usize;
    let mut fn_tap_layer = 0usize;
    let mut fn_press_time: Option<std::time::Instant> = None;
    let mut needs_complete_redraw = true;

    let mut input_tb = Libinput::new_with_udev(Interface);
    let mut input_main = Libinput::new_with_udev(Interface);
    input_tb.udev_assign_seat("seat-touchbar").unwrap();
    input_main.udev_assign_seat("seat0").unwrap();

    let udev_monitor = MonitorBuilder::new()
        .unwrap()
        .match_subsystem("power_supply")
        .unwrap()
        .listen()
        .unwrap();

    let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();
    epoll
        .add(input_main.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0))
        .unwrap();
    epoll
        .add(input_tb.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 1))
        .unwrap();
    epoll
        .add(cfg_mgr.fd(), EpollEvent::new(EpollFlags::EPOLLIN, 2))
        .unwrap();
    epoll
        .add(&udev_monitor, EpollEvent::new(EpollFlags::EPOLLIN, 3))
        .unwrap();
    if let Some(ref n) = niri {
        epoll.add(n, EpollEvent::new(EpollFlags::EPOLLIN, 4)).unwrap();
    }

    uinput.set_evbit(EventKind::Key).unwrap();
    for layer in &layers {
        for button in &layer.buttons {
            for k in &button.1.action {
                uinput.set_keybit(*k).unwrap();
            }
        }
    }

    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput
        .dev_setup(&uinput_setup {
            id: input_id {
                bustype: 0x19,
                vendor: 0x1209,
                product: 0x316E,
                version: 1,
            },
            ff_effects_max: 0,
            name: dev_name_c,
        })
        .unwrap();
    uinput.dev_create().unwrap();

    let mut digitizer: Option<InputDevice> = None;
    let mut touches: HashMap<i32, (usize, usize)> = HashMap::new();
    let mut last_redraw_ts = if layers[active_layer].faster_refresh {
        Local::now().second()
    } else {
        Local::now().minute()
    };

    // Poll live modules (vol/brt/wifi) every N seconds
    const LIVE_POLL_MS: u64 = 3000;
    let mut last_live_poll = std::time::Instant::now();

    loop {
        if cfg_mgr.update_config(&mut cfg, &mut layers, width) {
            active_layer = 0;
            fn_tap_layer = 0;
            needs_complete_redraw = true;
            if let Some(ref n) = niri {
                rebuild_info_layer(&mut layers, n);
            }
        }

        if let Some(ref mut n) = niri {
            if n.process_events() {
                rebuild_info_layer(&mut layers, n);
                if active_layer == 1 {
                    needs_complete_redraw = true;
                }
            }
        }

        if layers[active_layer].displays_live
            && last_live_poll.elapsed().as_millis() as u64 >= LIVE_POLL_MS
        {
            last_live_poll = std::time::Instant::now();
            for button in &mut layers[active_layer].buttons {
                if matches!(
                    button.1.image,
                    ButtonImage::Volume | ButtonImage::Brightness | ButtonImage::Wifi
                ) {
                    button.1.changed = true;
                }
            }
        }

        let now = Local::now();
        let ms_left = ((60 - now.second()) * 1000) as i32;
        let mut next_timeout_ms = min(ms_left, TIMEOUT_MS);

        if cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = pixel_shift.update();
            if pixel_shift_needs_redraw {
                needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, pixel_shift_next_timeout_ms);
        }

        let current_ts = if layers[active_layer].faster_refresh {
            Local::now().second()
        } else {
            Local::now().minute()
        };
        if layers[active_layer].displays_time && (current_ts != last_redraw_ts) {
            needs_complete_redraw = true;
            last_redraw_ts = current_ts;
        }

        if layers[active_layer].displays_battery {
            for button in &mut layers[active_layer].buttons {
                if let ButtonImage::Battery(_, _, _) = button.1.image {
                    button.1.changed = true;
                }
            }
        }

        if needs_complete_redraw || layers[active_layer].buttons.iter().any(|b| b.1.changed) {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let clips = layers[active_layer].draw(
                &cfg,
                width as i32,
                height as i32,
                &surface,
                shift,
                needs_complete_redraw,
            );
            let data = surface.data().unwrap();
            drm.map().unwrap().as_mut()[..data.len()].copy_from_slice(&data);
            drm.dirty(&clips).unwrap();
            needs_complete_redraw = false;
        }

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.unwrap(),
        };

        _ = udev_monitor.iter().last();

        input_tb.dispatch().unwrap();
        input_main.dispatch().unwrap();
        for event in &mut input_tb.clone().chain(input_main.clone()) {
            backlight.process_event(&event);
            match event {
                Event::Device(DeviceEvent::Added(evt)) => {
                    let dev = evt.device();
                    if dev.name().contains(" Touch Bar") {
                        digitizer = Some(dev);
                    }
                }
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    if key.key() == Key::Fn as u32 {
                        match key.key_state() {
                            KeyState::Pressed => {
                                fn_press_time = Some(std::time::Instant::now());
                                if layers.len() > 1 {
                                    active_layer = layers.len() - 1;
                                    needs_complete_redraw = true;
                                }
                            }
                            KeyState::Released => {
                                let was_tap = fn_press_time
                                    .take()
                                    .map(|t| t.elapsed().as_millis() < FN_TAP_THRESHOLD_MS)
                                    .unwrap_or(false);
                                if was_tap {
                                    fn_tap_layer = (fn_tap_layer + 1) % layers.len();
                                    active_layer = fn_tap_layer;
                                } else {
                                    active_layer = fn_tap_layer;
                                }
                                needs_complete_redraw = true;
                            }
                        }
                    }
                }
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer || backlight.current_bl() == 0 {
                        continue;
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(width as u32);
                            let y = dn.y_transformed(height as u32);
                            if let Some(btn) =
                                layers[active_layer].hit(width, height, x, y, None)
                            {
                                touches.insert(dn.seat_slot() as i32, (active_layer, btn));
                                let is_niri_ws = matches!(
                                    layers[active_layer].buttons[btn].1.image,
                                    ButtonImage::NiriWorkspace { .. }
                                );
                                if is_niri_ws {
                                    if let Some(ref mut n) = niri {
                                        if let Some(&(_, ws_idx)) = layers[active_layer]
                                            .niri_workspace_ids
                                            .iter()
                                            .find(|&&(bi, _)| bi == btn)
                                        {
                                            n.focus_workspace(ws_idx);
                                        }
                                    }
                                } else {
                                    layers[active_layer].buttons[btn]
                                        .1
                                        .set_active(&mut uinput, true);
                                }
                            }
                        }
                        TouchEvent::Motion(mtn) => {
                            if !touches.contains_key(&(mtn.seat_slot() as i32)) {
                                continue;
                            }
                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            let (layer, btn) = *touches.get(&(mtn.seat_slot() as i32)).unwrap();
                            let hit = layers[active_layer]
                                .hit(width, height, x, y, Some(btn))
                                .is_some();
                            layers[layer].buttons[btn].1.set_active(&mut uinput, hit);
                        }
                        TouchEvent::Up(up) => {
                            if !touches.contains_key(&(up.seat_slot() as i32)) {
                                continue;
                            }
                            let (layer, btn) = *touches.get(&(up.seat_slot() as i32)).unwrap();
                            layers[layer].buttons[btn].1.set_active(&mut uinput, false);
                            touches.remove(&(up.seat_slot() as i32));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        backlight.update_backlight(&cfg);
    }
}
