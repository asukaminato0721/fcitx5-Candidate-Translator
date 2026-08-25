#!/bin/zsh
set -euo pipefail

readonly project_root="${0:A:h:h}"
readonly command_name="${1:-build}"
readonly macos_arch="${MACOS_ARCH:-$(uname -m)}"
readonly deployment_target="13.3"
readonly plugin_name="candidate-translator"
readonly library_name="libfcitx5-candidate-translator.so"

case "$command_name" in
  build|test|package) ;;
  *)
    print -u2 "Usage: $0 [build|test|package]"
    exit 2
    ;;
esac

case "$macos_arch" in
  arm64)
    readonly rust_target="aarch64-apple-darwin"
    ;;
  x86_64)
    readonly rust_target="x86_64-apple-darwin"
    ;;
  *)
    print -u2 "Unsupported macOS architecture: $macos_arch"
    exit 2
    ;;
esac

if [[ "$(uname -s)" != "Darwin" ]]; then
  print -u2 "macOS packaging must run on macOS"
  exit 2
fi

prepare_sdk() {
  if [[ -n "${FCITX5_MACOS_ROOT:-}" ]]; then
    print -r -- "${FCITX5_MACOS_ROOT:A}"
    return
  fi

  local installed_root="/Library/Input Methods/Fcitx5.app/Contents"
  if [[ -f "$installed_root/include/Fcitx5/Core/fcitx/addonfactory.h" ]]; then
    print -r -- "$installed_root"
    return
  fi

  local release="${FCITX5_MACOS_RELEASE:-latest}"
  if [[ -z "$release" || "$release" == *[^A-Za-z0-9._-]* ]]; then
    print -u2 "Invalid FCITX5_MACOS_RELEASE: $release"
    exit 2
  fi

  local sdk_cache="$project_root/target/fcitx5-macos-dev/$release/$macos_arch"
  local sdk_root="$sdk_cache/Fcitx5.app/Contents"
  if [[ ! -f "$sdk_root/include/Fcitx5/Core/fcitx/addonfactory.h" ]]; then
    local archive="$sdk_cache/Fcitx5-$macos_arch-dev.tar.bz2"
    local download="$archive.download"
    mkdir -p "$sdk_cache"
    print -u2 "Downloading the official fcitx5-macos $release SDK for $macos_arch..."
    curl --retry 3 -fL \
      "https://github.com/fcitx-contrib/fcitx5-macos/releases/download/$release/Fcitx5-$macos_arch-dev.tar.bz2" \
      -o "$download"
    mv "$download" "$archive"
    tar xjf "$archive" -C "$sdk_cache"
  fi
  print -r -- "$sdk_root"
}

readonly sdk_root="$(prepare_sdk)"
readonly sdk_library="$sdk_root/lib"

if [[ ! -f "$sdk_library/libFcitx5Core.dylib" ]]; then
  print -u2 "Fcitx5 Core library not found under: $sdk_root"
  exit 2
fi

if command -v rustup >/dev/null 2>&1; then
  if ! rustup target list --installed | grep -qx "$rust_target"; then
    print -u2 "Rust target $rust_target is not installed. Run: rustup target add $rust_target"
    exit 2
  fi
elif [[ "$(rustc -vV | awk '/^host:/ {print $2}')" != "$rust_target" ]]; then
  print -u2 "Cross-compiling for $rust_target requires a Rust toolchain manager such as rustup"
  exit 2
fi

run_cargo() {
  FCITX5_MACOS_ROOT="$sdk_root" \
    CARGO_TARGET_DIR="$project_root/target" \
    MACOSX_DEPLOYMENT_TARGET="$deployment_target" \
    "$@"
}

run_tests() {
  DYLD_LIBRARY_PATH="$sdk_library${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
    run_cargo cargo test --locked --target "$rust_target"
}

build_release() {
  run_cargo cargo build --release --locked --target "$rust_target"
}

package_release() {
  build_release

  local cargo_library="$project_root/target/$rust_target/release/libfcitx5_candidate_translator.dylib"
  if [[ ! -f "$cargo_library" ]]; then
    print -u2 "Cargo output not found: $cargo_library"
    exit 1
  fi

  local output_dir="$project_root/target/macos-package"
  local staging
  staging="$(mktemp -d "${TMPDIR:-/tmp}/candidate-translator-package.XXXXXX")"
  trap "rm -rf -- '$staging'" EXIT

  local package_root="$staging/package"
  mkdir -p "$package_root/lib/fcitx5" "$package_root/plugin"
  mkdir -p "$package_root/share/fcitx5/addon"

  install -m 755 "$cargo_library" "$package_root/lib/fcitx5/$library_name"
  install_name_tool -id "@rpath/$library_name" \
    "$package_root/lib/fcitx5/$library_name"
  strip -x "$package_root/lib/fcitx5/$library_name"
  local packaged_library="$package_root/lib/fcitx5/$library_name"
  codesign --force --sign - "$packaged_library"
  lipo "$packaged_library" -verify_arch "$macos_arch"
  if ! nm -gU "$packaged_library" | grep -q '_fcitx_addon_factory_instance$'; then
    print -u2 "Packaged library does not export fcitx_addon_factory_instance"
    exit 1
  fi
  if otool -L "$packaged_library" | grep -Eq '/usr/local|/opt/homebrew'; then
    print -u2 "Packaged library unexpectedly depends on Homebrew"
    exit 1
  fi
  if ! otool -l "$packaged_library" | grep -Fq \
    '/Library/Input Methods/Fcitx5.app/Contents/lib'; then
    print -u2 "Packaged library is missing the Fcitx5.app runtime search path"
    exit 1
  fi
  codesign --verify --strict "$packaged_library"
  install -m 644 "$project_root/data/candidate-translator.conf" \
    "$package_root/share/fcitx5/addon/candidate-translator.conf"

  local binary_version
  local data_version
  binary_version="$(shasum -a 256 "$packaged_library" | awk '{print $1}')"
  data_version="$(shasum -a 256 "$package_root/share/fcitx5/addon/candidate-translator.conf" | awk '{print $1}')"
  local descriptor
  descriptor="{\"files\":[\"lib/fcitx5/$library_name\",\"share/fcitx5/addon/candidate-translator.conf\"],\"version\":\"$binary_version\",\"data_version\":\"$data_version\"}"
  print -rn -- "$descriptor" > "$package_root/plugin/$plugin_name.json"

  mkdir -p "$output_dir"
  local archive="$output_dir/$plugin_name-$macos_arch.tar.bz2"
  rm -f "$output_dir/$plugin_name-any.tar.bz2"
  COPYFILE_DISABLE=1 tar --no-xattrs -cjf "$archive" \
    -C "$package_root" lib plugin share

  print "Created: $archive"
}

cd "$project_root"
case "$command_name" in
  build) build_release ;;
  test) run_tests ;;
  package) package_release ;;
esac
