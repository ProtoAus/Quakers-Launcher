#!/bin/bash
# =============================================================================
# Build every Linux x86_64 artifact the Quakers alpha ships:
#   engine client      fteqw-gl64
#   dedicated server   fteqw-sv64
#   physics plugin     fteplug_box3d_amd64.so     <- quakers/default.cfg plug_loads this
#   HL2 asset plugin   fteplug_hl2_amd64.so
#   CoD asset plugin   fteplug_cod_amd64.so
#   launcher           quakers-launcher
#
# Everything lands in  C:\FTEQuake\_engine\linux64\ , which is where publish.py
# picks up the linux64 half of the manifest.
#
# RUN IT FROM WINDOWS LIKE THIS (note MSYS_NO_PATHCONV, or git-bash mangles /mnt/...):
#     MSYS_NO_PATHCONV=1 wsl -d Ubuntu-22.04 -- bash /mnt/c/FTEQuake/launcher/deploy/build-linux.sh
#
# One-time setup inside WSL (see linux-build.md for the why):
#     sudo apt-get install -y build-essential pkg-config git curl unzip zip nasm cmake rsync \
#       zlib1g-dev libpng-dev libjpeg-dev libfreetype6-dev libogg-dev libvorbis-dev \
#       libspeex-dev libspeexdsp-dev libopus-dev libgl1-mesa-dev libglu1-mesa-dev \
#       libvulkan-dev libx11-dev libxext-dev libxrandr-dev libxcursor-dev libxxf86vm-dev \
#       libxi-dev libxkbcommon-dev libasound2-dev libpulse-dev libsdl2-dev
#     curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
# =============================================================================
set -e

FTE_SRC=/mnt/c/msys64/home/Lex/fteqw
BOX3D_SRC=/mnt/c/msys64/home/Lex/box3d-main
LAUNCHER_SRC=/mnt/c/FTEQuake/launcher
OUT=/mnt/c/FTEQuake/_engine/linux64

FTE=$HOME/fteqw
BOX3D=$HOME/box3d-main
LAUNCHER=$HOME/quakers-launcher
JOBS=$(nproc)

# Everything is built on the WSL native filesystem, never in place on /mnt/c:
#   - cargo/make on /mnt/c is 5-20x slower (9p filesystem, per-file stat cost)
#   - a Linux `cargo build --release` would otherwise clobber the Windows
#     target/release/ binaries, since both use the same path without --target
mkdir -p "$OUT"

say() { echo; echo "=============== $* ==============="; }

# ---------------------------------------------------------------- box3d ------
# Box3D (Erin Catto's C fork of Box2D) is pure C with no install step. The Windows
# side builds it by hand into build_manual/; we mirror that exactly so the FTE
# plugin rule finds $(BOX3D_BASE)/build_manual/libbox3d.a.
say "libbox3d.a"
mkdir -p "$BOX3D"
rsync -a --delete --exclude 'build/' --exclude 'build_manual/' --exclude '.git/' "$BOX3D_SRC/" "$BOX3D/"
mkdir -p "$BOX3D/build_manual"
cd "$BOX3D/build_manual"
rm -f ./*.o ./libbox3d.a
# -std=gnu17: box3d needs C17 (_Static_assert, anonymous unions). SSE2 is x86_64 baseline,
# which is all box3d's simd.c asks for, so no -mavx2 etc. is required.
ls "$BOX3D"/src/*.c | xargs -P "$JOBS" -I{} \
    gcc -c -std=gnu17 -O2 -DNDEBUG -fPIC -I"$BOX3D/include" -I"$BOX3D/src" {}
ar rcs libbox3d.a ./*.o
echo "libbox3d.a: $(stat -c %s libbox3d.a) bytes, $(ar t libbox3d.a | wc -l) objects"

# ---------------------------------------------------------------- engine -----
say "FTE engine (client + dedicated server)"
mkdir -p "$FTE"
rsync -a --delete \
    --exclude '.git/' --exclude '*.o' --exclude '*.d' --exclude '*.exe' --exclude '*.dll' \
    --exclude 'release/' --exclude 'debug/' --exclude 'libs-x86_64-w64-mingw32/' \
    "$FTE_SRC/" "$FTE/"
cd "$FTE/engine"
# a CRLF Makefile checked out on Windows breaks GNU make
file Makefile | grep -q CRLF && sed -i 's/\r$//' Makefile
# the openxr plugin rule cds into this dir; create it even though we don't build openxr
mkdir -p "$FTE/engine/libs-x86_64-linux-gnu"

make gl-rel FTE_TARGET=linux64 -j"$JOBS"
make sv-rel FTE_TARGET=linux64 -j"$JOBS"

say "FTE plugins (box3d, hl2, cod)"
# Only the three quakers/default.cfg actually plug_loads. The full default plugin set
# drags in openxr/ffmpeg and is not worth the build surface.
# ODE is deliberately NOT built: box3d is the default backend and the only one the mod
# ships. `sv_physics_engine ode` therefore has no effect on Linux.
make plugins-rel FTE_TARGET=linux64 \
     NATIVE_PLUGINS="box3d hl2 cod" \
     BOX3D_BASE="$BOX3D" \
     -j"$JOBS"

# ---------------------------------------------------------------- launcher ---
say "quakers-launcher"
source "$HOME/.cargo/env"
mkdir -p "$LAUNCHER"
rsync -a --delete --exclude 'target/' --exclude 'dist/' "$LAUNCHER_SRC/" "$LAUNCHER/"
cd "$LAUNCHER"
cargo build --release

# ---------------------------------------------------------------- collect ----
say "collecting into $OUT"
# --remove-destination is NOT optional. publish.py populates dist/objects/<hh>/<hash> with
# HARDLINKS to these files. A plain `cp -f` opens the destination with O_TRUNC and writes
# through the shared inode, which rewrites the already-published blob in place while it keeps
# its old hash-derived name -- a content-addressed store that silently stops being immutable.
# Unlinking first breaks the hardlink, so old objects keep the bytes they were named for.
cd "$FTE/engine/release"
cp -f --remove-destination fteqw-gl64 fteqw-sv64 \
      fteplug_box3d_amd64.so fteplug_hl2_amd64.so fteplug_cod_amd64.so "$OUT/"
cp -f --remove-destination "$LAUNCHER/target/release/quakers-launcher" "$OUT/"

say "RESULT"
ok=0
for f in fteqw-gl64 fteqw-sv64 fteplug_box3d_amd64.so fteplug_hl2_amd64.so \
         fteplug_cod_amd64.so quakers-launcher; do
    if [ -f "$OUT/$f" ]; then
        printf "  OK       %-28s %10s bytes\n" "$f" "$(stat -c %s "$OUT/$f")"
    else
        printf "  MISSING  %s\n" "$f"; ok=1
    fi
done
echo
echo "  next, on Windows:  python C:\\FTEQuake\\launcher\\publish.py --prune"
exit $ok
