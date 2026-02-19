use crate::fonts::{FontConfig, Pattern};
use crate::FunctionLayer;
use anyhow::Error;
use cairo::FontFace;
use freetype::Library as FtLibrary;
use input_linux::Key;
use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor},
};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};
use std::{fmt, fs::read_to_string, os::fd::AsFd};

const USER_CFG_PATH: &str = "/etc/tiny-dfr/config.toml";

pub struct Theme {
    pub background:       (f64, f64, f64),
    pub foreground:       (f64, f64, f64),
    pub button_inactive:  (f64, f64, f64),
    pub button_active:    (f64, f64, f64),
    pub accent:           (f64, f64, f64), // focused workspace, active indicators
    pub success:          (f64, f64, f64), // battery charging
    pub warning:          (f64, f64, f64), // battery low
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            background:      (0.0,   0.0,   0.0),
            foreground:      (1.0,   1.0,   1.0),
            button_inactive: (0.200, 0.200, 0.200),
            button_active:   (0.400, 0.400, 0.400),
            accent:          (0.0,   0.514, 0.761), // base16 blue-ish
            success:         (0.216, 0.663, 0.216), // base16 green-ish
            warning:         (0.859, 0.196, 0.196), // base16 red-ish
        }
    }
}

fn hex_to_rgb(s: &str) -> Option<(f64, f64, f64)> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0))
}

pub struct Config {
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_face: FontFace,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub theme: Theme,
}

fn build_theme(
    background: Option<String>, foreground: Option<String>,
    button_inactive: Option<String>, button_active: Option<String>,
    accent: Option<String>, success: Option<String>, warning: Option<String>,
) -> Theme {
    let d = Theme::default();
    Theme {
        background:      background.as_deref().and_then(hex_to_rgb).unwrap_or(d.background),
        foreground:      foreground.as_deref().and_then(hex_to_rgb).unwrap_or(d.foreground),
        button_inactive: button_inactive.as_deref().and_then(hex_to_rgb).unwrap_or(d.button_inactive),
        button_active:   button_active.as_deref().and_then(hex_to_rgb).unwrap_or(d.button_active),
        accent:          accent.as_deref().and_then(hex_to_rgb).unwrap_or(d.accent),
        success:         success.as_deref().and_then(hex_to_rgb).unwrap_or(d.success),
        warning:         warning.as_deref().and_then(hex_to_rgb).unwrap_or(d.warning),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    #[allow(dead_code)]
    media_layer_default: Option<bool>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_template: Option<String>,
    adaptive_brightness: Option<bool>,
    theme_background:      Option<String>,
    theme_foreground:      Option<String>,
    theme_button_inactive: Option<String>,
    theme_button_active:   Option<String>,
    theme_accent:          Option<String>,
    theme_success:         Option<String>,
    theme_warning:         Option<String>,
    active_brightness: Option<u32>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    // Info layer is always present but starts empty; niri fills it at runtime.
    // This field is kept for TOML override/fallback only.
    info_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
}

fn array_or_single<'de, D>(deserializer: D) -> Result<Vec<Key>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ArrayOrSingle;

    impl<'de> Visitor<'de> for ArrayOrSingle {
        type Value = Vec<Key>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("string or array of strings")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<Key>, E> {
            Ok(vec![Deserialize::deserialize(
                de::value::BorrowedStrDeserializer::new(value),
            )?])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Vec<Key>, A::Error> {
            Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))
        }
    }

    deserializer.deserialize_any(ArrayOrSingle)
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct ButtonConfig {
    #[serde(alias = "Svg")]
    pub icon: Option<String>,
    pub text: Option<String>,
    pub theme: Option<String>,
    pub time: Option<String>,
    pub battery: Option<String>,
    pub locale: Option<String>,
    #[serde(deserialize_with = "array_or_single", default)]
    pub action: Vec<Key>,
    pub stretch: Option<usize>,
    // special dynamic button types for the info layer
    pub niri_workspaces: Option<bool>,
    pub niri_window_title: Option<bool>,
}

fn load_font(name: &str) -> FontFace {
    let fontconfig = FontConfig::new();
    let mut pattern = Pattern::new(name);
    fontconfig.perform_substitutions(&mut pattern);
    let pat_match = match fontconfig.match_pattern(&pattern) {
        Ok(pat) => pat,
        Err(_) => panic!("Unable to find specified font. If you are using the default config, make sure you have at least one font installed")
    };
    let file_name = pat_match.get_file_name();
    let file_idx = pat_match.get_font_index();
    let ft_library = FtLibrary::init().unwrap();
    let face = ft_library.new_face(file_name, file_idx).unwrap();
    FontFace::create_from_ft(&face).unwrap()
}

fn load_config(width: u16) -> (Config, Vec<FunctionLayer>) {
    let mut base =
        toml::from_str::<ConfigProxy>(&read_to_string("/usr/share/tiny-dfr/config.toml").unwrap())
            .unwrap();
    let user = read_to_string(USER_CFG_PATH)
        .map_err::<Error, _>(|e| e.into())
        .and_then(|r| Ok(toml::from_str::<ConfigProxy>(&r)?));
    if let Ok(user) = user {
        base.media_layer_default = user.media_layer_default.or(base.media_layer_default);
        base.show_button_outlines = user.show_button_outlines.or(base.show_button_outlines);
        base.enable_pixel_shift = user.enable_pixel_shift.or(base.enable_pixel_shift);
        base.font_template = user.font_template.or(base.font_template);
        base.adaptive_brightness = user.adaptive_brightness.or(base.adaptive_brightness);
        base.media_layer_keys = user.media_layer_keys.or(base.media_layer_keys);
        base.info_layer_keys = user.info_layer_keys.or(base.info_layer_keys);
        base.primary_layer_keys = user.primary_layer_keys.or(base.primary_layer_keys);
        base.active_brightness = user.active_brightness.or(base.active_brightness);
        base.theme_background      = user.theme_background.or(base.theme_background);
        base.theme_foreground      = user.theme_foreground.or(base.theme_foreground);
        base.theme_button_inactive = user.theme_button_inactive.or(base.theme_button_inactive);
        base.theme_button_active   = user.theme_button_active.or(base.theme_button_active);
        base.theme_accent          = user.theme_accent.or(base.theme_accent);
        base.theme_success         = user.theme_success.or(base.theme_success);
        base.theme_warning         = user.theme_warning.or(base.theme_warning);
    };

    let mut media_layer_keys = base.media_layer_keys.unwrap();
    let mut primary_layer_keys = base.primary_layer_keys.unwrap();

    let mut info_layer_keys = base.info_layer_keys.unwrap_or_else(|| {
        vec![
            ButtonConfig {
                niri_workspaces: Some(true),
                stretch: None,
                icon: None, text: None, theme: None, time: None,
                battery: None, locale: None, action: vec![],
                niri_window_title: None,
            },
            ButtonConfig {
                niri_window_title: Some(true),
                stretch: Some(6),
                icon: None, text: None, theme: None, time: None,
                battery: None, locale: None, action: vec![],
                niri_workspaces: None,
            },
            ButtonConfig {
                time: Some("%a %b %d %I:%M:%S %p".into()),
                stretch: Some(4),
                icon: None, text: None, theme: None,
                battery: None, locale: None, action: vec![],
                niri_workspaces: None, niri_window_title: None,
            },
        ]
    });

    if width >= 2170 {
        for layer in [&mut media_layer_keys, &mut info_layer_keys, &mut primary_layer_keys] {
            layer.insert(
                0,
                ButtonConfig {
                    icon: None,
                    text: Some("esc".into()),
                    theme: None,
                    action: vec![Key::Esc],
                    stretch: None,
                    time: None,
                    locale: None,
                    battery: None,
                    niri_workspaces: None,
                    niri_window_title: None,
                },
            );
        }
    }

    let fkey_layer = FunctionLayer::with_config(primary_layer_keys);
    let mut info_layer = FunctionLayer::with_config(info_layer_keys.clone());
    // stored so rebuild_info_layer can re-expand dynamic entries on niri state changes
    info_layer.source_config = info_layer_keys;
    let media_layer = FunctionLayer::with_config(media_layer_keys);

    // Fixed order: 0 = F-keys, 1 = Info, 2 = Media
    let layers = vec![fkey_layer, info_layer, media_layer];

    let theme = build_theme(
        base.theme_background, base.theme_foreground,
        base.theme_button_inactive, base.theme_button_active,
        base.theme_accent, base.theme_success, base.theme_warning,
    );
    let cfg = Config {
        show_button_outlines: base.show_button_outlines.unwrap(),
        enable_pixel_shift: base.enable_pixel_shift.unwrap(),
        adaptive_brightness: base.adaptive_brightness.unwrap(),
        font_face: load_font(&base.font_template.unwrap()),
        active_brightness: base.active_brightness.unwrap(),
        theme,
    };
    (cfg, layers)
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    watch_desc: Option<WatchDescriptor>,
}

fn arm_inotify(inotify_fd: &Inotify) -> Option<WatchDescriptor> {
    let flags = AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CLOSE | AddWatchFlags::IN_ONESHOT;
    match inotify_fd.add_watch(USER_CFG_PATH, flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

impl ConfigManager {
    pub fn new() -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let watch_desc = arm_inotify(&inotify_fd);
        ConfigManager {
            inotify_fd,
            watch_desc,
        }
    }
    pub fn load_config(&self, width: u16) -> (Config, Vec<FunctionLayer>) {
        load_config(width)
    }
    pub fn update_config(
        &mut self,
        cfg: &mut Config,
        layers: &mut Vec<FunctionLayer>,
        width: u16,
    ) -> bool {
        if self.watch_desc.is_none() {
            self.watch_desc = arm_inotify(&self.inotify_fd);
            return false;
        }
        match self.inotify_fd.read_events() {
            Err(Errno::EAGAIN) => false,
            r => self.handle_events(cfg, layers, width, r),
        }
    }
    #[cold]
    fn handle_events(&mut self, cfg: &mut Config, layers: &mut Vec<FunctionLayer>, width: u16, evts: Result<Vec<InotifyEvent>, Errno>) -> bool {
        let mut ret = false;
        for evt in evts.unwrap() {
            if Some(evt.wd) != self.watch_desc {
                continue;
            }
            let parts = load_config(width);
            *cfg = parts.0;
            *layers = parts.1;
            ret = true;
            self.watch_desc = arm_inotify(&self.inotify_fd);
        }
        ret
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}
