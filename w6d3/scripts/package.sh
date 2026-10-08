#!/bin/sh
# Сборка RagLab.app: release-бинарник Rust, config.json с путями, иконка из icon.svg, electron-packager.
# Из Finder/Dock у приложения нет PATH и рабочей папки терминала, поэтому каталог дня зашивается в config.json.
set -e
cd "$(dirname "$0")/.."
cargo build --release
printf '{ "root": "%s" }\n' "$(pwd)" > config.json

rm -rf build && mkdir -p build/icon.iconset
npx electron scripts/icon.js "$(pwd)/icon.svg" "$(pwd)/build/icon.png"
for size in 16 32 128 256 512; do
  sips -z $size $size build/icon.png --out "build/icon.iconset/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) build/icon.png --out "build/icon.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns build/icon.iconset -o build/icon.icns

npx electron-packager . RagLab --platform=darwin --arch=arm64 --out=release --overwrite --no-asar \
  --icon=build/icon.icns \
  --ignore='^/(release|build|target|src|data|scripts|Cargo\.(toml|lock)|README\.md|icon\.svg)'
echo "Готово: $(pwd)/release/RagLab-darwin-arm64/RagLab.app"
