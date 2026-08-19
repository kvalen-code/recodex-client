#!/usr/bin/env bash
# ReCodex 的 macOS 打包:单个 .app + DMG。
#
# 不复用上游 package-dmg.sh:那份打两个 app(含已从工作区移除的管理工具)
# 并写死 Codex++ 品牌与 URL scheme。这里只保留 ReCodex 需要的部分。
#
# 未签名/未公证:Gatekeeper 会拦"已损坏,无法打开"。要正式发给用户,
# 需要 Apple 开发者账号做 codesign + notarytool。ad-hoc 签名只保证本机能跑。
set -euo pipefail

VERSION="${1:?用法: package-recodex-dmg.sh <版本号> <arch>}"
ARCH="${2:-$(uname -m)}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DIST="$ROOT/dist/macos"
STAGE="$DIST/stage"
BINARY_DIR="${BINARY_DIR:-$ROOT/target/release}"
BINARY="$BINARY_DIR/codex-plus-plus"
APP_NAME="ReCodex"
BUNDLE_ID="com.recodex.desktop"
DMG="$DIST/ReCodex-${VERSION}-macos-${ARCH}.dmg"
ICON_SOURCE="$ROOT/apps/codex-plus-manager/src-tauri/icons/icon.png"
ICON_NAME="recodex.icns"

rm -rf "$DIST"
mkdir -p "$STAGE"

if [ ! -x "$BINARY" ]; then
  echo "error: 找不到可执行文件或不可执行: $BINARY" >&2
  exit 1
fi

# --- 图标 -------------------------------------------------------------------
ICONSET="$DIST/recodex.iconset"
mkdir -p "$ICONSET"
for spec in "16 icon_16x16" "32 icon_16x16@2x" "32 icon_32x32" "64 icon_32x32@2x" \
            "128 icon_128x128" "256 icon_128x128@2x" "256 icon_256x256" \
            "512 icon_256x256@2x" "512 icon_512x512" "1024 icon_512x512@2x"; do
  size="${spec%% *}"; name="${spec##* }"
  sips -z "$size" "$size" "$ICON_SOURCE" --out "$ICONSET/$name.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$DIST/$ICON_NAME"

# --- .app -------------------------------------------------------------------
APP_DIR="$STAGE/$APP_NAME.app"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$BINARY" "$APP_DIR/Contents/MacOS/ReCodex"
chmod +x "$APP_DIR/Contents/MacOS/ReCodex"
cp "$DIST/$ICON_NAME" "$APP_DIR/Contents/Resources/$ICON_NAME"
printf 'APPL????' > "$APP_DIR/Contents/PkgInfo"

cat > "$APP_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleSignature</key><string>????</string>
  <key>CFBundleExecutable</key><string>ReCodex</string>
  <key>CFBundleIconFile</key><string>$ICON_NAME</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSUIElement</key><false/>
</dict>
</plist>
PLIST

# ad-hoc 签名。先签可执行文件再签 bundle —— 顺序反了 bundle 签名会失效。
codesign --force --sign - "$APP_DIR/Contents/MacOS/ReCodex"
codesign --force --sign - "$APP_DIR"

# --- 自检:装出来的东西必须真能跑 -------------------------------------------
plutil -lint "$APP_DIR/Contents/Info.plist" >/dev/null
test -x "$APP_DIR/Contents/MacOS/ReCodex"
codesign -dv "$APP_DIR" >/dev/null 2>&1
# 版本号必须与传入的一致,否则装完客户端自报旧版本会永远提示有更新
got="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_DIR/Contents/Info.plist")"
[ "$got" = "$VERSION" ] || { echo "error: Info.plist 版本 $got != $VERSION" >&2; exit 1; }

# --- DMG --------------------------------------------------------------------
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
test -f "$DMG"
echo "已生成: $DMG"
