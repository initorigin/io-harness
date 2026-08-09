#!/usr/bin/env bash
# Type-check and lint the `cfg`-gated platform code that this build host cannot
# compile, without waiting for the CI matrix.
#
# WHY THIS EXISTS
#
# The development host is macOS. `src/sandbox/landlock.rs`, `src/sandbox/seccomp.rs`
# and the `cfg(windows)` half of `src/sandbox/appcontainer.rs` are compiled by no
# local gate at all — not `cargo check`, not `cargo clippy -D warnings`, not the
# test suite. `cargo check --target <triple>` does not close that: rusqlite is
# built with `bundled`, so cross-compiling the crate needs a cross C toolchain
# that is not installed and is not worth installing.
#
# 0.47.0 paid for this three times in one afternoon — a function that was never
# re-exported out of its private module, six clippy errors in Linux-only code,
# and a signature change that only broke a `cfg(windows)` test. Each cost a full
# matrix round. Each of them is caught by this script in seconds.
#
# HOW IT WORKS
#
# The platform modules are leaves: they depend on `libc` or `windows-sys` and on
# a handful of the crate's own types. So each is compiled *standalone* with
# `rustc --emit=metadata` against a shim that supplies those types, for the real
# target triple. No linking, no C, no cargo.
#
# The shim deliberately reaches every item through the **module path** the real
# caller uses, because the one defect this script originally missed was a
# function that compiled fine and was private to its module.
#
# WHAT IT DOES NOT COVER
#
# The `cfg`-gated code in `src/sandbox/linux.rs` and `src/sandbox/windows.rs`,
# which depends on the whole crate and cannot be extracted this cheaply. Those
# still need the matrix. Run this first anyway: it is seconds against minutes,
# and it catches the leaf modules where most of the unsafe code lives.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

LINUX_TARGETS=(x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu)
WINDOWS_TARGET=x86_64-pc-windows-msvc

note() { printf '\n\033[1m%s\033[0m\n' "$*"; }
have_target() { rustup target list --installed | grep -qx "$1"; }

# Copy a module, eliding its `tracing::` calls.
#
# The shims link no `tracing`, and pulling it in for a foreign target drags its
# own dependency graph along for no benefit: a logging statement is not what any
# of this is checking. Every other line is compiled exactly as it ships, which is
# the property that matters — a shim that rewrote logic could pass while the real
# file does not.
elide_tracing() {
    python3 -c '
import re, sys
src = open(sys.argv[1]).read()
src = re.sub(r"^([ \t]*)tracing::(warn|debug|info|trace|error)!\((?:[^()]|\([^()]*\))*\);?[ \t]*$",
             r"\1{}", src, flags=re.M)
open(sys.argv[2], "w").write(src)
' "$1" "$2"
}

# --- dependency metadata, per target ----------------------------------------
# `libc` and `windows-sys` are pure Rust, so building them alone for a foreign
# target needs no C compiler. The final link fails on Windows for want of an MSVC
# linker; the `.rmeta` is produced first and is all this script needs.
dep_rmeta() {
    local target="$1"
    local crate="$2"
    local manifest="$WORK/dep-$crate-$target"
    mkdir -p "$manifest/src"
    case "$crate" in
    libc) echo 'libc = "0.2"' >"$manifest/dep.toml" ;;
    windows-sys) cat >"$manifest/dep.toml" <<'TOML'
windows-sys = { version = "0.61", features = [
    "Win32_Foundation","Win32_Security","Win32_Security_Authorization","Win32_Security_Isolation",
    "Win32_System_Diagnostics_ToolHelp","Win32_System_JobObjects","Win32_System_Threading",
] }
TOML
        ;;
    esac
    {
        echo '[package]'
        echo 'name = "depprobe"'
        echo 'version = "0.0.0"'
        echo 'edition = "2021"'
        echo '[dependencies]'
        cat "$manifest/dep.toml"
    } >"$manifest/Cargo.toml"
    echo 'fn main(){}' >"$manifest/src/main.rs"
    (cd "$manifest" && cargo build --target "$target" >/dev/null 2>&1) || true
    local under="${crate//-/_}"
    ls "$manifest/target/$target/debug/deps/lib${under}"-*.rmeta 2>/dev/null | head -1
}

fail=0

# --- Linux: landlock + seccomp ----------------------------------------------
cat >"$WORK/linux_shim.rs" <<'RS'
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode { ReadOnly, WorkspaceWrite, FullAccess }
pub mod linux { pub const LANDLOCK_NET_ABI: u32 = 4; }
#[path = "landlock.rs"] pub mod landlock;
#[path = "seccomp.rs"] pub mod seccomp;

// Every item reached through the MODULE path, exactly as `sandbox/linux.rs`
// reaches it. Without this the shim type-checks the bodies and misses an item
// that was never re-exported out of its private `imp` module — which is the
// defect that made this script necessary.
#[allow(dead_code)]
fn consumer(fd: std::os::fd::RawFd, w: &std::path::Path) -> std::io::Result<()> {
    let abi = landlock::abi().unwrap_or(1);
    let p = landlock::plan(abi, ExecMode::WorkspaceWrite, true, w, &[], w);
    let r = landlock::Ruleset::build(&p)?;
    let _ = r.raw();
    unsafe { landlock::restrict_self(fd)?; }
    unsafe { seccomp::install()?; }
    Ok(())
}
fn main() {}
RS
sed 's|use super::ExecMode;|use crate::ExecMode;|; s|super::linux::LANDLOCK_NET_ABI|crate::linux::LANDLOCK_NET_ABI|g' \
    "$ROOT/src/sandbox/landlock.rs" >"$WORK/landlock.rs"
elide_tracing "$ROOT/src/sandbox/seccomp.rs" "$WORK/seccomp.rs"

for target in "${LINUX_TARGETS[@]}"; do
    if ! have_target "$target"; then
        echo "skip $target (rustup target add $target)"
        continue
    fi
    rmeta="$(dep_rmeta "$target" libc)"
    if [ -z "$rmeta" ]; then
        echo "skip $target (no libc metadata)"
        continue
    fi
    deps="$(dirname "$rmeta")"
    note "rustc  $target"
    rustc --edition 2021 --target "$target" --emit=metadata --crate-type bin --test -A warnings \
        --extern libc="$rmeta" -L "$deps" \
        -o "$WORK/out-$target.rmeta" "$WORK/linux_shim.rs" || fail=1
    note "clippy $target"
    clippy-driver --edition 2021 --target "$target" --emit=metadata --crate-type bin -D warnings \
        --extern libc="$rmeta" -L "$deps" \
        -o "$WORK/clip-$target.rmeta" "$WORK/linux_shim.rs" || fail=1
done

# --- Windows: appcontainer, tests included ----------------------------------
if have_target "$WINDOWS_TARGET"; then
    rmeta="$(dep_rmeta "$WINDOWS_TARGET" windows-sys)"
    if [ -n "$rmeta" ]; then
        deps="$(dirname "$rmeta")"
        cat >"$WORK/win_shim.rs" <<'RS'
pub mod shim {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Grant { ReadExecute, Full }
    pub struct TempDir(std::path::PathBuf);
    impl TempDir { pub fn path(&self) -> &std::path::Path { &self.0 } }
    pub fn tempdir() -> std::io::Result<TempDir> { Ok(TempDir(std::path::PathBuf::new())) }
}
#[path = "appcontainer.rs"] pub mod appcontainer;
fn main() {}
RS
        # The module's own test block is KEPT: a signature change that only breaks
        # a `cfg(windows)` test is one of the three defects this script exists for.
        sed -e 's|crate::sandbox::windows::Grant|crate::shim::Grant|g' \
            -e 's|tempfile::tempdir()|crate::shim::tempdir()|g' \
            "$ROOT/src/sandbox/appcontainer.rs" >"$WORK/ac_raw.rs"
        elide_tracing "$WORK/ac_raw.rs" "$WORK/appcontainer.rs"
        note "rustc  $WINDOWS_TARGET (with tests)"
        rustc --edition 2021 --target "$WINDOWS_TARGET" --emit=metadata --crate-type bin --test -A warnings \
            --extern windows_sys="$rmeta" -L "$deps" \
            -o "$WORK/out-win.rmeta" "$WORK/win_shim.rs" || fail=1
    else
        echo "skip $WINDOWS_TARGET (no windows-sys metadata)"
    fi
else
    echo "skip $WINDOWS_TARGET (rustup target add $WINDOWS_TARGET)"
fi

if [ "$fail" -ne 0 ]; then
    note "cross-check FAILED — fix these before pushing, they are matrix rounds"
    exit 1
fi
note "cross-check clean"
