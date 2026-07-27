# quakers launcher + distribution

A Minecraft-style launcher and content-distribution setup for playtesting **quakers** (~6.3 GB) with
~100 testers, without saturating the proto.bar Pi's 40 Mbps uplink (which also runs the live game server).

Ships **Windows x64 and Linux x64** from one manifest and one object tree.

## How it fits together

```
  dev machine                         mirror                        tester
  -----------                         ------                        ------
  publish.py  --rclone-->  dl.proto.bar                       <-->  launcher.exe
   (manifest +              = Cloudflare edge (cache)                (Rust TUI, resumable,
    objects/)                  -> Cloudflare R2 `quakers-dl`          verify + repair)
                             $0 egress, no home uplink in the path
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
- **Mirrors** are tried in order per file with automatic failover, but only **one** is configured:
  `dl.proto.bar` → Cloudflare **R2**. A second entry is worth adding when a second *host* exists —
  a hostname that does not resolve is not a failover, it is a guaranteed connect timeout per file.
  **The `/objects/` Cache Rule is required, not an optimisation:** blobs are hash-named with no
  file extension, and Cloudflare's default caching is extension-driven, so without the rule every
  request comes back `cf-cache-status: DYNAMIC`.
- **Why not just put the CDN in front of the Pi?** That was the setup until 2026-07-27, when one
  tester's install saturated the home uplink. The launcher had *not* bypassed Cloudflare — every
  origin request came from a Cloudflare edge IP. But the edge cache is per-*server*, and the
  launcher runs 8 parallel workers that land on different machines in the same PoP; each missed
  and fetched from origin independently. The Pi served **6.20 GB to deliver a 5.84 GB payload**
  (2.96× re-fetch). A CDN in front of a home connection does not remove the home connection from
  the path. Enabling **Smart Tiered Cache** narrows it; moving the origin to R2 ends it.
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
| **Cloudflare R2** (`quakers-dl`) | **Live, verified** — 4,112 objects / 5.85 GB; `dl.proto.bar` is an R2 custom domain. A 32-file fetch through it added **0 lines** to the Pi's access log |
| Cache Rule on `/objects/` | **Live** — verified MISS→HIT after the R2 cutover, including a 474 MB part. Note `curl -I` reports `DYNAMIC` on HEAD even when cached; judge it on GETs |
| Pi mirror (nginx off the NVMe) | **Retired from serving.** Content still there, vhost kept but `limit_rate`d, and no DNS points at it. See `deploy/rclone-and-r2-setup.md` §0 for why |
| Manifest signing + launcher self-update + code-signing | **TODO** (M3) |

> **Cache ceiling.** Cloudflare's free plan will not cache a single object over 512 MB, so the
> texture pk3s are split to stay under it (`tools/split_pk3.py` in the mod repo). An oversized
> pack is not an error — it just silently never caches and hits the Pi every time.

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
quakers-launcher.exe -y              # skip the confirmation prompt (scripts/CI)
```
Linux (same flags):
```
chmod +x quakers-launcher && ./quakers-launcher
```
**One file is all a tester needs.** Drop `quakers-launcher.exe` in an empty folder and run it —
every setting has a compiled-in default, so there is no companion file to lose. `launcher.toml`
is an optional override (see below), not something to hand out.

It prints where it is about to
install, how many files and how many bytes, and waits for you to confirm before writing anything.
Enter accepts (`Download to <dir>?`, or `Continue download?` if a partial install is already
there); Esc/n/q backs out. With no interactive terminal — piped, redirected, CI — it proceeds
rather than blocking forever. Kill it mid-download and re-run: it resumes each file from where it
stopped (HTTP Range).
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

### `launcher.toml` (optional)

Never required. Place it next to the exe (or in the install dir) to override the compiled-in
defaults — useful for pointing one tester at a test mirror without building them a binary:

```toml
mirrors     = ["https://dl.proto.bar"]   # tried before the manifest's own list
channel     = "alpha"                    # picks manifests/<channel>.json
install_dir = "."                        # only honoured from the file beside the exe
concurrency = 8                          # parallel download workers
```

Precedence is **CLI flag > launcher.toml > compiled-in default > manifest**, per setting. A
missing or malformed file is silently ignored rather than an error — that is what makes the
single-file distribution work. `install_dir` is read only from the exe-adjacent copy, since it
decides where the *other* candidate file would be.

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
