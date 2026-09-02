#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}"
state_dir="${XDG_STATE_HOME:-$HOME/.local/state}"

rm -f -- "$bin_dir/omarchy-mail"
rm -f -- "$data_dir/applications/org.omarchy.Mail.desktop"
rm -f -- "$data_dir/metainfo/org.omarchy.Mail.metainfo.xml"
rm -f -- "$data_dir/icons/hicolor/scalable/apps/org.omarchy.Mail.svg"

template="$config_dir/omarchy/themed/omarchy-mail.css.tpl"
if [[ -f $template ]] && cmp -s "$project_dir/packaging/omarchy-mail.css.tpl" "$template"; then
  rm -f -- "$template"
elif [[ -e $template ]]; then
  printf 'Preserved modified theme template at %s\n' "$template"
fi

rm -f -- "$state_dir/omarchy/current/theme/omarchy-mail.css"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$data_dir/applications" >/dev/null 2>&1 || true
fi

printf 'Removed the Omarchy Mail development install. Email data was left untouched.\n'
