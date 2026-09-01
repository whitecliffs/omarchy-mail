#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}"

cd "$project_dir"
cargo build --release --locked
install -Dm755 target/release/omarchy-mail "$bin_dir/omarchy-mail"
install -Dm644 packaging/org.omarchy.Mail.desktop "$data_dir/applications/org.omarchy.Mail.desktop"
install -Dm644 packaging/icons/hicolor/scalable/apps/org.omarchy.Mail.svg "$data_dir/icons/hicolor/scalable/apps/org.omarchy.Mail.svg"
install -Dm644 packaging/omarchy-mail.css.tpl "$config_dir/omarchy/themed/omarchy-mail.css.tpl"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$data_dir/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [[ -d "$data_dir/icons/hicolor" ]]; then
  gtk-update-icon-cache -f -t "$data_dir/icons/hicolor" >/dev/null 2>&1 || true
fi

printf 'Installed Omarchy Mail to %s\n' "$bin_dir/omarchy-mail"
printf 'Theme template installed at %s\n' "$config_dir/omarchy/themed/omarchy-mail.css.tpl"
printf 'Preview with: OMARCHY_MAIL_DEMO=1 %q\n' "$bin_dir/omarchy-mail"
