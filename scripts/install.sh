#!/usr/bin/env bash
# Build MyMail in release mode and install it.
#   macOS: /Applications/MyMail.app (override with APP_DIR=...)
#   Linux: ~/.local/bin/mymail plus a desktop entry and icon
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --locked
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

case "$(uname -s)" in
Darwin)
  app="target/release/MyMail.app"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp target/release/mymail "$app/Contents/MacOS/mymail"
  cp assets/MyMail.icns "$app/Contents/Resources/MyMail.icns"
  cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>MyMail</string>
  <key>CFBundleDisplayName</key><string>MyMail</string>
  <key>CFBundleIdentifier</key><string>com.github.tonibergholm.mymail</string>
  <key>CFBundleExecutable</key><string>mymail</string>
  <key>CFBundleIconFile</key><string>MyMail</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
  # Ad-hoc signature so macOS runs the locally built bundle.
  codesign --force --deep --sign - "$app"
  dest="${APP_DIR:-/Applications}/MyMail.app"
  rm -rf "$dest"
  ditto "$app" "$dest"
  echo "Installed $dest"
  ;;
Linux)
  install -Dm755 target/release/mymail "$HOME/.local/bin/mymail"
  install -Dm644 assets/icon.png "$HOME/.local/share/icons/hicolor/512x512/apps/mymail.png"
  install -Dm644 /dev/stdin "$HOME/.local/share/applications/mymail.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=MyMail
Comment=Fast, minimal Gmail client
Exec=$HOME/.local/bin/mymail
Icon=mymail
Categories=Network;Email;
StartupWMClass=mymail
EOF
  echo "Installed ~/.local/bin/mymail and the MyMail desktop entry"
  ;;
*)
  echo "Unsupported OS: $(uname -s)" >&2
  exit 1
  ;;
esac
