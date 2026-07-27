# quakers launcher + distribution

A Minecraft-style launcher and content-distribution setup for playtesting **quakers** (~6.3 GB) with
~100 testers, without saturating the proto.bar Pi's 40 Mbps uplink (which also runs the live game server).

Ships **Windows x64 and Linux x64** from one manifest and one object tree.

## How it fits together

```
  dev machine                      mirrors                         tester
  -----------                      -------                         ------
  publish.py  --rclone sync-->  Cloudflare R2 (primary, $0 egress) <--\
   (manifest +                                                          launcher.exe
    objects/)  --rclone sync-->  proto.bar Pi / Caddy (failover)    <--/  (Rust TUI, resumable,
                                                                          verify + repair, self-update)
```

- **Content-addressed:** every shippable file is stored by its hash (`objects/<hh>/<hash>`). Unchanged
  files dedupe across releases and are cached forever; a patch uploads/downloads only what changed.
- **Manifest** (`manifests/<channel>.json`) lists every file's `path / size / hash`. The launcher diffs
  local files against it: download the missing, re-fetch the corrupt (repair).
- **One manifest covers every platform.** 4,034 files (6.27 GB) of maps/textures/models/progs are
  identical everywhere and carry no `platform` tag; only the engine set differs — 6 files / 11.4 MB
  for `win64`, 5 files / 10.5 MB for `linux64`. The launcher installs `platform == "all"` plus its
  own key, so **adding Linux cost 10.5 MB of storage and zero extra bytes for a Windows tester.**
  Entries flagged `"exec": true` get `chmod +x` on unix — without that the engine downloads
  byte-perfect and then won't start.
- **Mirrors** are tried in order per file with automatic failover. R2 absorbs the launch spike for $0;
  the Pi is a rate-limited backup so live play is never starved.
- FTE's own connect-time downloader stays on as a last-resort safety net (see `default.fmf`).

## What ships vs. what doesn't

`publish.py` assembles a clean player install from `C:\FTEQuake` + `C:\FTEQuake\quakers`
(+ `C:\FTEQuake\_engine\linux64` for the Linux engine set):
- **Ships (4,045 files / 6.28 GB):** the texture pk3s, loose `textures/` + `models/` + `maps/`,
  gfx/sounds/particles/glsl/data, the compiled `.dat`s, cfgs, and both engine binary sets.
  World textures ship as BC7 `.dds` **inside** the pk3s; a loose `textures/*.png` whose stem is
  already in a pk3 is dropped automatically, and anything under a `disable/` directory is skipped.
- **Skipped:** `src/ tools/ _prerender_backup/ _staging/`, source meshes (`.obj/.iqe/.acd/.smd/.mtl/.cmd`),
  the **editor-only** prop PNGs (the game reads the BC7 `.dds` inside the pk3s), logs/dumps/debug
  symbols, `id1/` (commercial). CS/HL/CoD content is never shipped — it's mounted from the tester's own
  installed games via `quakers/fs_addons.txt`.

Audit any run in `dist/included.txt` and `dist/excluded.txt`.

## Status

| Piece | State |
|---|---|
| `publish.py` (staging + manifest + object tree + `--prune`) | **Done, verified** — 6.28 GB / 4,045 files across both platforms |
| `quakers/textures/` trim (drop pk3-covered loose textures) | **Done** — dropped 2,861 (~1.9 GB), kept 1,059 not in any pk3 |
| Rust launcher, Windows (`quakers-launcher.exe`) | **Done, tested** — sync/resume(206)/repair/failover/dry-run all pass |
| **Rust launcher, Linux** (`quakers-launcher`) | **Done, tested** — 4.1 MB ELF; links only libc/libm/libgcc (rustls+ring, *no* OpenSSL) |
| **Linux engine** (`fteqw-gl64`, `fteqw-sv64`, box3d/hl2/cod plugins) | **Done, tested** — `deploy/build-linux.sh`; dedicated server boots the mod, loads box3d, spawns a map |
| **Multi-platform manifest** (schema 2) | **Done, tested** — platform filtering + exec-bit verified end-to-end against a local mirror |
| Pi mirror (nginx static off the NVMe) | **Staged** — objects+manifests on `/srv/nvme/quakers/dl`; see `deploy/nginx-quakers-dl.conf` |
| Cloudflare R2 (primary, launch-spike) | **DNS done, bucket TODO** — see `deploy/rclone-and-r2-setup.md` |
| Manifest signing + launcher self-update + code-signing | **TODO** (M3) |

> **Not in git.** This directory is not a repository. The QC mod at `quakers/src` is versioned;
> the launcher, `publish.py` and `deploy/` are not. Worth `git init`-ing before the alpha.

> **Why not MinIO:** the Pi's MinIO stores data on the 7 GB root overlay, too small for the
> 6.3 GB payload. The NVMe (66 GB free) serves the tree statically via nginx — same HTTP GETs
> the launcher needs. See `deploy/nginx-quakers-dl.conf`.

## Launcher usage (tester)

Windows:
```
quakers-launcher.exe                 # read launcher.toml, sync, then launch the game
quakers-launcher.exe --verify        # full hash check + repair any corrupt/altered file
quakers-launcher.exe --no-launch     # just update
quakers-launcher.exe --dry-run       # show what would download, then exit
```
Linux (same flags):
```
chmod +x quakers-launcher && ./quakers-launcher
```
Drop the launcher + `launcher.toml` in an empty folder and run — it fills the folder with the
game. Kill it mid-download and re-run: it resumes each file from where it stopped (HTTP Range).
The launcher sets `+x` on the engine binaries it downloads, so the Linux tester only ever has to
chmod the launcher itself.

**Linux runtime deps.** The launcher itself needs nothing but glibc. The *engine* dynamically links
the usual media libraries, and dlopen()s GL/X11/ALSA at runtime. On a bare Debian/Ubuntu box:
```
sudo apt install libgl1 libx11-6 libxrandr2 libxcursor1 libxxf86vm1 libasound2 \
                 libvorbisfile3 libogg0 libspeex1 libspeexdsp1 libopus0 \
                 libfreetype6 libpng16-16 libjpeg-turbo8 zlib1g
```
Most desktop installs already have all of these.

## Usage (dev machine)

```
# 1. Linux artifacts (engine + plugins + launcher), built in WSL:
MSYS_NO_PATHCONV=1 wsl -d Ubuntu-22.04 -- bash /mnt/c/FTEQuake/launcher/deploy/build-linux.sh

# 2. Windows launcher:
cargo build --release

# 3. Manifest + object tree for both platforms:
python publish.py --prune
python publish.py --report-only              # fast: classify + size only, no hashing
python publish.py --platforms win64          # Windows-only release
python publish.py --channel stable --version 2026.07.27_1

# 4. Push with rclone (see deploy/rclone-and-r2-setup.md)
```
### ⚠ Do not switch the hash algorithm after the first upload

Object names *are* content hashes, so changing `hash_algo` renames **every** blob and forces a full
6.8 GB re-upload plus a full re-download for every tester.

**This project is committed to BLAKE2b-256.** `publish.py` will silently prefer BLAKE3 if the
`blake3` module is ever installed — so **don't `pip install blake3`** on the publishing machine.
The speedup is meaningless here (a publish is ~15 s cold and ~1 s warm off the mtime cache) and
BLAKE2b-256 is in the Python stdlib, so any machine can publish with no dependencies.

## Next

1. Create the R2 bucket + custom domain and push (`deploy/rclone-and-r2-setup.md`), then validate
   with `curl`.
2. Add manifest signing + launcher self-update + code-sign the exe (dodges SmartScreen).
3. macOS, if wanted: the manifest schema already has room for a `macos64` key — it needs an
   `arm64`/`x86_64` engine build and a notarised launcher.
