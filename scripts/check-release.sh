#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

for command in cargo makepkg pkg-config; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'Missing required release tool: %s\n' "$command" >&2
    exit 1
  }
done

for file in \
  packaging/PKGBUILD \
  packaging/org.omarchy.Mail.desktop \
  packaging/org.omarchy.Mail.metainfo.xml \
  packaging/icons/hicolor/scalable/apps/org.omarchy.Mail.svg \
  packaging/omarchy-mail.css.tpl; do
  [[ -f $file ]] || {
    printf 'Missing packaging file: %s\n' "$file" >&2
    exit 1
  }
done

bash -n scripts/install-dev.sh scripts/uninstall-dev.sh scripts/check-release.sh
cargo fmt --all -- --check
cargo test --locked

pushd packaging >/dev/null
makepkg --printsrcinfo >/dev/null
popd >/dev/null

if command -v desktop-file-validate >/dev/null 2>&1; then
  desktop-file-validate packaging/org.omarchy.Mail.desktop
else
  printf 'Skipping desktop-file-validate (install desktop-file-utils for package linting).\n'
fi

if command -v appstreamcli >/dev/null 2>&1; then
  appstreamcli validate --no-net --strict packaging/org.omarchy.Mail.metainfo.xml
else
  printf 'Skipping appstreamcli (install appstream for AppStream linting).\n'
fi

printf 'Release checks passed.\n'
