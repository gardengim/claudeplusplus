#!/bin/sh
# Installs claude++ (the name) to ~/.local/bin. Cargo and rustc reject '+' in a binary name, so the build
# artifact is called claudeplusplus; it is installed as claude++, with claudeplusplus kept as a fallback alias for
# shells or tools where '++' is a problem.
set -e
cd "$(dirname "$0")"
cargo build --release
dir="${PREFIX:-$HOME/.local/bin}"
mkdir -p "$dir"
install -m 755 target/release/claudeplusplus "$dir/claude++"
ln -sf claude++ "$dir/claudeplusplus"
# the old alias, if an earlier install made it
[ "$(readlink "$dir/claudepp" 2>/dev/null)" = "claude++" ] && rm "$dir/claudepp"
true
echo "installed $dir/claude++ (alias: claudeplusplus)"
