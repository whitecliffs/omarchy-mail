use gtk::gdk::Display;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct ThemePalette {
    pub mode: String,
    pub background: String,
    pub dark_background: String,
    pub darker_background: String,
    pub lighter_background: String,
    pub foreground: String,
    pub light_foreground: String,
    pub muted: String,
    pub accent: String,
    pub selection: String,
    pub red: String,
    pub yellow: String,
    pub green: String,
    pub blue: String,
    pub cyan: String,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            mode: "dark".into(),
            background: "#111c18".into(),
            dark_background: "#0c1512".into(),
            darker_background: "#090f0d".into(),
            lighter_background: "#23372b".into(),
            foreground: "#c1c497".into(),
            light_foreground: "#d6d5bc".into(),
            muted: "#53685b".into(),
            accent: "#509475".into(),
            selection: "#32473b".into(),
            red: "#ff5345".into(),
            yellow: "#e5c736".into(),
            green: "#549e6a".into(),
            blue: "#509475".into(),
            cyan: "#2dd5b7".into(),
        }
    }
}

impl ThemePalette {
    pub fn load() -> Self {
        let path = current_colors_path();
        let content = fs::read_to_string(path).unwrap_or_default();
        let values = content
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| {
                (
                    key.trim().to_string(),
                    value.trim().trim_matches('"').to_string(),
                )
            })
            .collect::<HashMap<_, _>>();
        let defaults = Self::default();
        Self {
            mode: values.get("mode").cloned().unwrap_or(defaults.mode),
            background: color(&values, "background", &defaults.background),
            dark_background: color(&values, "dark_background", &defaults.dark_background),
            darker_background: color(&values, "darker_background", &defaults.darker_background),
            lighter_background: color(&values, "lighter_background", &defaults.lighter_background),
            foreground: color(&values, "foreground", &defaults.foreground),
            light_foreground: color(&values, "light_foreground", &defaults.light_foreground),
            muted: color(&values, "muted", &defaults.muted),
            accent: color(&values, "accent", &defaults.accent),
            selection: color(&values, "selection", &defaults.selection),
            red: color(&values, "red", &defaults.red),
            yellow: color(&values, "yellow", &defaults.yellow),
            green: color(&values, "green", &defaults.green),
            blue: color(&values, "blue", &defaults.blue),
            cyan: color(&values, "cyan", &defaults.cyan),
        }
    }

    pub fn css(&self) -> String {
        format!(
            r#"
@define-color om-background {};
@define-color om-dark-background {};
@define-color om-darker-background {};
@define-color om-lighter-background {};
@define-color om-foreground {};
@define-color om-light-foreground {};
@define-color om-muted {};
@define-color om-accent {};
@define-color om-selection {};
@define-color om-red {};
@define-color om-yellow {};
@define-color om-green {};
@define-color om-blue {};
@define-color om-cyan {};

window.omarchy-mail-window {{
  background: @om-background;
  color: @om-foreground;
}}

.mail-shell {{ background: @om-background; color: @om-foreground; }}
.mail-sidebar {{ background: @om-dark-background; border-right: 1px solid alpha(@om-muted, 0.28); }}
.mail-middle {{ background: @om-background; border-right: 1px solid alpha(@om-muted, 0.28); }}
.mail-reader {{ background: @om-background; }}
.mail-section-label {{ color: @om-muted; font-size: 0.78em; font-weight: 700; letter-spacing: 0.06em; }}
.mail-sidebar-row {{ min-height: 34px; border-radius: 8px; color: @om-foreground; }}
.mail-sidebar-row:hover {{ background: alpha(@om-lighter-background, 0.62); }}
.mail-sidebar-row.selected {{ background: @om-selection; color: @om-light-foreground; }}
.mail-sidebar-row.selected label, .mail-sidebar-row.selected image, .mail-sidebar-row.selected .mail-count {{ color: @om-light-foreground; }}
.mail-count {{ color: @om-muted; font-size: 0.86em; }}
.mail-filter-bar {{ background: @om-dark-background; border-bottom: 1px solid alpha(@om-muted, 0.24); }}
.mail-message-row {{ min-height: 86px; border-bottom: 1px solid alpha(@om-muted, 0.17); padding: 8px 14px; }}
.mail-message-row > * {{ min-width: 0; }}
.mail-message-row:hover {{ background: alpha(@om-lighter-background, 0.42); }}
.mail-message-row:selected {{ background: @om-selection; }}
.mail-message-row.unread .mail-sender, .mail-message-row.unread .mail-subject {{ color: @om-light-foreground; font-weight: 700; }}
.mail-sender {{ color: @om-foreground; }}
.mail-subject {{ color: @om-foreground; }}
.mail-preview, .mail-date, .mail-recipient {{ color: @om-muted; }}
.mail-unread-dot {{ color: @om-accent; font-size: 1.15em; }}
.mail-star {{ color: @om-yellow; }}
.mail-attachment {{ color: @om-muted; }}
.mail-reader-subject {{ color: @om-light-foreground; font-size: 1.5em; font-weight: 700; }}
.mail-reader-sender {{ color: @om-light-foreground; font-weight: 700; }}
.mail-reader-meta {{ color: @om-muted; }}
.mail-reader-body {{ color: @om-foreground; font-size: 1.03em; line-height: 1.5; background: transparent; border: none; padding: 0; }}
.mail-reader-body text {{ color: @om-foreground; background: transparent; }}
.mail-html-document {{ color: @om-foreground; }}
.mail-html-table {{ min-width: 0; }}
.mail-html-row {{ min-width: 0; }}
.mail-html-cell {{ min-width: 0; }}
.mail-html-header-cell {{ font-weight: 700; }}
.mail-html-blockquote {{ border-left: 3px solid alpha(@om-muted, 0.45); color: @om-muted; }}
.mail-format-button {{ min-width: 30px; min-height: 30px; padding: 3px 8px; }}
.mail-format-button:hover {{ background: alpha(@om-selection, 0.7); }}
.mail-settings-signature {{ background: alpha(@om-dark-background, 0.35); border: 1px solid alpha(@om-muted, 0.22); border-radius: 7px; padding: 5px 7px; }}
.mail-inline-images {{ background: alpha(@om-dark-background, 0.34); border: 1px solid alpha(@om-muted, 0.2); border-radius: 10px; padding: 10px 12px; }}
.mail-remote-image-notice {{ background: alpha(@om-selection, 0.3); border: 1px solid alpha(@om-muted, 0.2); border-radius: 10px; padding: 9px 12px; }}
.mail-inline-image {{ border-radius: 8px; }}
.mail-conversation-label {{ color: @om-muted; font-size: 0.9em; font-weight: 700; }}
.mail-conversation-expander {{ background: alpha(@om-selection, 0.32); border-radius: 8px; color: @om-light-foreground; }}
.mail-attachment-chip {{ background: alpha(@om-selection, 0.8); border: 1px solid alpha(@om-accent, 0.35); border-radius: 8px; padding: 6px 10px; color: @om-light-foreground; }}
.mail-empty-title {{ color: @om-light-foreground; font-size: 1.4em; font-weight: 700; }}
.mail-empty-body {{ color: @om-muted; }}
.mail-accent-button {{ background: @om-accent; color: @om-darker-background; font-weight: 700; }}
.mail-accent-button:hover {{ background: @om-light-foreground; }}
.mail-status {{ color: @om-muted; font-size: 0.86em; }}
.mail-account-title {{ color: @om-light-foreground; font-weight: 700; }}
.mail-danger {{ color: @om-red; }}
.mail-dialog {{ background: @om-dark-background; }}
"#,
            self.background,
            self.dark_background,
            self.darker_background,
            self.lighter_background,
            self.foreground,
            self.light_foreground,
            self.muted,
            self.accent,
            self.selection,
            self.red,
            self.yellow,
            self.green,
            self.blue,
            self.cyan,
        )
    }

    fn fingerprint(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.mode, self.background, self.foreground, self.accent, self.selection
        )
    }
}

pub fn install() -> Rc<gtk::CssProvider> {
    let provider = Rc::new(gtk::CssProvider::new());
    apply(&provider, &ThemePalette::load());
    if let Some(display) = Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            provider.as_ref(),
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    watch(provider.clone());
    provider
}

fn apply(provider: &gtk::CssProvider, palette: &ThemePalette) {
    provider.load_from_data(&palette.css());
}

fn watch(provider: Rc<gtk::CssProvider>) {
    let mut fingerprint = ThemePalette::load().fingerprint();
    glib::timeout_add_local(Duration::from_millis(900), move || {
        let palette = ThemePalette::load();
        let next = palette.fingerprint();
        if next != fingerprint {
            apply(provider.as_ref(), &palette);
            fingerprint = next;
        }
        glib::ControlFlow::Continue
    });
}

fn color(values: &HashMap<String, String>, key: &str, fallback: &str) -> String {
    values
        .get(key)
        .filter(|value| value.starts_with('#'))
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

fn current_colors_path() -> PathBuf {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    state_home.join("omarchy/current/theme/colors.toml")
}

#[allow(dead_code)]
fn _system_time(path: &PathBuf) -> Option<SystemTime> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}
