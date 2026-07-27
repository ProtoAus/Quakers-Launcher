# Building the Linux side of the Quakers alpha

Everything is built in **WSL2 Ubuntu 22.04** and lands in `C:\FTEQuake\_engine\linux64\`, which is
where `publish.py` picks up the `linux64` half of the manifest.

```
MSYS_NO_PATHCONV=1 wsl -d Ubuntu-22.04 -- bash /mnt/c/FTEQuake/launcher/deploy/build-linux.sh
```

`MSYS_NO_PATHCONV=1` is not optional from git-bash: without it, MSYS rewrites `/mnt/c/...` into a
Windows path and the script is never found.

## What it produces

| Artifact | Size | What it is |
|---|---|---|
| `fteqw-gl64` | 6.4 MB | OpenGL client — what testers run |
| `fteqw-sv64` | 3.3 MB | dedicated server |
| `fteplug_box3d_amd64.so` | 1.0 MB | **physics backend** — every prop and ragdoll |
| `fteplug_hl2_amd64.so` | 206 KB | Half-Life 2 / Source asset loader (VBSP, VTF) |
| `fteplug_cod_amd64.so` | 86 KB | Call of Duty asset loader |
| `quakers-launcher` | 4.1 MB | the launcher itself |

## One-time WSL setup

```bash
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential pkg-config git curl unzip zip nasm cmake rsync \
  zlib1g-dev libpng-dev libjpeg-dev libfreetype6-dev \
  libogg-dev libvorbis-dev libspeex-dev libspeexdsp-dev libopus-dev \
  libgl1-mesa-dev libglu1-mesa-dev libvulkan-dev \
  libx11-dev libxext-dev libxrandr-dev libxcursor-dev libxxf86vm-dev \
  libxi-dev libxkbcommon-dev libasound2-dev libpulse-dev libsdl2-dev

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

`zip` is easy to miss and gives a confusing failure: the FTE plugin rules shell out to it to embed
plugin metadata, so without it every plugin dies with `/bin/sh: 1: zip: not found` (**Error 127**)
*after* compiling successfully.

## Things that bit us, so they don't bite again

**Build on the WSL filesystem, never in place on `/mnt/c`.** The script rsyncs the sources to
`~/fteqw`, `~/box3d-main` and `~/quakers-launcher` first. Two reasons: `/mnt/c` is 5–20× slower for
build workloads, and a Linux `cargo build --release` in `C:\FTEQuake\launcher` would **overwrite the
Windows `target/release/` binaries** — both use the same path when no `--target` triple is given.

**Box3D must be built first.** `quakers/default.cfg` does `plug_load box3d`, so the physics plugin is
not optional. FTE's plugin rule links a prebuilt static `libbox3d.a` from
`$(BOX3D_BASE)/build_manual/`, mirroring how the Windows side does it. Box3D is pure C: the script
compiles all 49 `src/*.c` with `-std=gnu17 -O2 -fPIC` (it needs C17 for `_Static_assert` and
anonymous unions; SSE2 is x86_64 baseline, which is all its `simd.c` asks for) and `ar`s them.

**A `-static` fix was needed in the engine tree.** `plugins/Makefile` passed `-static` alongside
`-shared`, which is fine for a mingw DLL but makes GNU ld pull `crtbeginT.o` and fail with
`relocation R_X86_64_32 against hidden symbol '__TMC_END__' can not be used when making a shared
object`. There is now a `PLUG_FULLSTATIC` variable that is `-static` only on win targets; the ODE
and Box3D rules use it. **The Windows build is byte-for-byte unaffected.**

**Only three plugins are built.** `NATIVE_PLUGINS="box3d hl2 cod"` — the three `default.cfg` loads.
The Makefile's default set drags in openxr (which wants a download into a directory that does not
exist) and ffmpeg, neither of which the game uses.

**ODE is deliberately not built for Linux.** `sv_physics_engine ode` is an opt-in alternate backend;
box3d is the default and the only one shipped. On Linux that cvar silently has no effect. The
harmless console line `Plugin ode does not appear to be loaded` at server start is the mod's own
`plug_close ode` (`server/sv_main.qc:1620`) tidying up a backend that was never loaded.

## ⚠ glibc floor: 2.34

Built on Ubuntu 22.04, the binaries require **GLIBC_2.34**. glibc is backward-compatible but not
forward-compatible, so anything older simply refuses to start.

Check the build's requirement with
`objdump -T <binary> | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1`,
and a tester's machine with `ldd --version`.

### Supported

| Distro | glibc |
|---|---|
| Ubuntu 22.04 LTS / 24.04 LTS / 25.04 | 2.35 / 2.39 / 2.41 |
| Debian 12 *bookworm* / 13 *trixie* | 2.36 / 2.41 |
| Linux Mint 21.x / 22.x | 2.35 / 2.39 (Ubuntu base) |
| Pop!_OS 22.04 | 2.35 |
| Zorin OS 17, elementary OS 7 | 2.35 (Ubuntu 22.04 base) |
| Fedora 40 / 41 / 42 | 2.39 / 2.40 / 2.41 |
| RHEL / Rocky / Alma 9 | 2.34 — exactly at the floor, works |
| RHEL / Rocky / Alma 10 | 2.39 |
| openSUSE Leap 15.6 | 2.38 |
| openSUSE Tumbleweed | rolling |
| Arch, Manjaro, EndeavourOS, CachyOS, Garuda | rolling (2.4x) |
| **SteamOS 3.5+ (Steam Deck)** | 2.37+ (Arch base) |
| Bazzite, Nobara, ChimeraOS | Fedora/Arch base, current |
| Gentoo, NixOS, Void (glibc flavour) | current |
| Kali, MX Linux 23 | Debian 12/testing base |

### Not supported

| Distro | glibc | |
|---|---|---|
| Ubuntu 20.04 LTS | 2.31 | EOL for standard support since Apr 2025 |
| Debian 11 *bullseye* | 2.31 | |
| RHEL / Rocky / Alma 8 | 2.28 | |
| openSUSE Leap 15.5 | 2.31 | |
| Linux Mint 20.x | 2.31 | Ubuntu 20.04 base |
| **Alpine, Void (musl), Chimera Linux** | **n/a — musl** | Not a glibc version problem: musl is a different libc entirely, so **no** glibc build will ever run. Would need a separate musl target. |

Derived distros inherit their base's glibc — Mint/Pop!\_OS/Zorin/elementary follow Ubuntu, MX/Kali
follow Debian, Bazzite/Nobara follow Fedora, SteamOS/Manjaro/CachyOS follow Arch. When in doubt,
`ldd --version` settles it in one second.

To drop the floor to 2.31 (adds Ubuntu 20.04, Debian 11, Mint 20, Leap 15.5), build in an Ubuntu
20.04 container or WSL distro — the build scripts are unchanged, only the host moves. Nothing else
about the pipeline cares.

If testers on older LTS releases matter, build in an Ubuntu 20.04 container (or add a 20.04 WSL
distro) to drop the floor to 2.31. For a 2026 alpha aimed at current desktops and the Steam Deck,
2.34 is a reasonable line.

## Runtime dependencies on the tester's machine

The launcher needs nothing but glibc — it is statically linked apart from `libc`/`libm`/`libgcc_s`,
because we build it against **rustls + ring** rather than the default native-tls (which would drag in
OpenSSL and tie the binary to the build host's `libssl.so.3`).

The engine links the usual media libraries and `dlopen()`s GL/X11/ALSA at runtime:

```
sudo apt install libgl1 libx11-6 libxrandr2 libxcursor1 libxxf86vm1 libasound2 \
                 libvorbisfile3 libogg0 libspeex1 libspeexdsp1 libopus0 \
                 libfreetype6 libpng16-16 libjpeg-turbo8 zlib1g
```

Most desktop installs already satisfy this. There is no `sqlite3` shim in the Linux set — FTE
`dlopen()`s the system `libsqlite3.so.0`.

## Verifying a build

The dedicated server is the quickest real check — it exercises the engine, the progs and the physics
plugin together:

```bash
mkdir -p /tmp/qsrv && cd /tmp/qsrv
cp /mnt/c/FTEQuake/_engine/linux64/* .
cp /mnt/c/FTEQuake/default.fmf .
ln -sfn /mnt/c/FTEQuake/quakers quakers
timeout -s INT 40 ./fteqw-sv64 -basedir /tmp/qsrv +map <somemap>
```

A good run prints `Box3D: hull …` lines (the physics plugin building convex hulls for props — proof
box3d loaded), then `Server spawned.` and `======== FTE Initialized ========`.
