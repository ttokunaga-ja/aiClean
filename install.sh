#!/bin/sh
# Intended public entry point; available only after the repository and release are published.
# curl -fsSL https://raw.githubusercontent.com/ttokunaga-ja/aiClean/main/install.sh | sh
set -eu
if [ "$(uname -s)" != Darwin ]; then
  echo 'install.sh supports macOS. Use install.ps1 on Windows.' >&2; exit 1
fi
bin=${BIN_DIR:-"$HOME/.local/bin"}
case "$bin" in /*) ;; *) echo 'BIN_DIR must be absolute.' >&2; exit 1 ;; esac
case "$bin" in *:*|*'
'*) echo 'BIN_DIR cannot contain a colon or newline.' >&2; exit 1 ;; esac
tmp=$(mktemp -d)
stage=
trap 'rm -rf "$tmp"; if [ -n "$stage" ]; then rm -f "$stage"; fi' EXIT
trap 'exit 1' HUP INT TERM
fetch() {
  curl --disable --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 120 --max-filesize 134217728 "$@"
}
# Resolve once, then fetch the manifest and executable from the same immutable tag.
if ! release=$(fetch --head -o /dev/null -w '%{url_effective}' https://github.com/ttokunaga-ja/aiClean/releases/latest); then
  echo 'No published aiClean release is available. Build from the local source checkout with cargo build --release --locked.' >&2; exit 1
fi
tag=${release#https://github.com/ttokunaga-ja/aiClean/releases/tag/}
if [ "$tag" = "$release" ] || ! printf '%s\n' "$tag" | LC_ALL=C grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'; then
  echo 'Release tag must be vX.Y.Z.' >&2; exit 1
fi
base=https://github.com/ttokunaga-ja/aiClean/releases/download/$tag
fetch -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
# Validate every entry and reject duplicate names, including unrelated entries.
if ! LC_ALL=C awk '
  {hash=substr($0,1,64); marker=substr($0,66,1); name=substr($0,67)}
  length($0)<67 || length(hash)!=64 || hash !~ /^[0-9a-fA-F]+$/ || substr($0,65,1)!=" " || (marker!=" " && marker!="*") {exit 1}
  name=="" || name ~ /[[:space:]]/ || index(name,"/") || index(name,"\\") || seen[name]++ {exit 1}
  name=="aiClean-macos" {print; found++}
  END {if(found!=1) exit 1}
' "$tmp/SHA256SUMS" > "$tmp/match"; then
  echo 'SHA256SUMS contains malformed, duplicate, or missing entries.' >&2; exit 1
fi
fetch -o "$tmp/aiClean-macos" "$base/aiClean-macos"
if ! (cd "$tmp" && shasum -a 256 -c match >/dev/null); then
  echo 'Downloaded executable SHA-256 mismatch. Existing installation preserved.' >&2; exit 1
fi
mkdir -p "$bin"
if [ -L "$bin/aiClean" ] || { [ -e "$bin/aiClean" ] && [ ! -f "$bin/aiClean" ]; }; then
  echo 'Destination is not a regular file. Existing installation preserved.' >&2; exit 1
fi
stage=$(mktemp "$bin/.aiClean-install.XXXXXXXX")
cp "$tmp/aiClean-macos" "$stage"
chmod 755 "$stage"
"$stage" --version > "$tmp/version" &
version_pid=$!
(
  ticks=0
  while kill -0 "$version_pid" 2>/dev/null; do
    ticks=$((ticks+1))
    if [ "$ticks" -ge 100 ]; then kill -KILL "$version_pid" 2>/dev/null || :; exit; fi
    sleep 0.1
  done
) &
watchdog=$!
valid=1
wait "$version_pid" || valid=0
kill "$watchdog" 2>/dev/null || :
wait "$watchdog" 2>/dev/null || :
version=$(cat "$tmp/version")
if [ "$valid" != 1 ] || [ "$version" != "aiClean ${tag#v}" ] || [ "$(wc -l < "$tmp/version" | tr -d ' ')" != 1 ]; then
  echo 'Executable version did not match the release tag. Existing installation preserved.' >&2; exit 1
fi
mv -f "$stage" "$bin/aiClean"
stage=
echo "Installed: $bin/aiClean ($version)"
if [ "${AICLEAN_INSTALL_NO_PATH:-0}" != 1 ]; then
  case "${SHELL:-}" in
    */zsh) profile=${ZDOTDIR:-"$HOME"}/.zshrc ;;
    */bash)
      if [ -f "$HOME/.bash_profile" ]; then profile=$HOME/.bash_profile
      elif [ -f "$HOME/.bash_login" ]; then profile=$HOME/.bash_login
      elif [ -f "$HOME/.profile" ]; then profile=$HOME/.profile
      else profile=$HOME/.bash_profile; fi ;;
    *) profile=$HOME/.profile ;;
  esac
  quoted=$(printf '%s' "$bin" | sed "s/'/'\\\\''/g")
  entry="case :\$PATH: in *:'$quoted':*) ;; *) export PATH='$quoted':\$PATH ;; esac # aiClean installer"
  if [ ! -f "$profile" ] || ! grep -Fqx "$entry" "$profile"; then
    mkdir -p "$(dirname "$profile")"
    printf '\n%s\n' "$entry" >> "$profile"
  fi
  echo "PATH configured in $profile. Open a new terminal to use aiClean."
fi
