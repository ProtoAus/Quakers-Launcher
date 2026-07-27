#!/usr/bin/env python3
"""
publish.py -- build a nettest player-install manifest + content-addressed object tree
for distribution via Cloudflare R2 (primary) + the proto.bar Pi (failover).

It walks the engine install root and the nettest gamedir, applies the ship/skip rules,
hashes every shippable file, and writes:

    <out>/manifests/<channel>.json      the manifest (path / size / hash / component)
    <out>/objects/<hh>/<hash>           content-addressed blobs (hardlinked from source)
    <out>/included.txt, excluded.txt    audit lists

Nothing is uploaded here. Once the mirrors exist, publish with rclone (see deploy/):
    rclone sync <out>/objects   r2:nettest-dl/objects   --checksum --transfers=16
    rclone sync <out>/objects   pi:/srv/nettest/objects --checksum
    rclone copy <out>/manifests r2:nettest-dl/manifests            # publish LAST (atomic)
    rclone copy <out>/manifests pi:/srv/nettest/manifests

The manifest records `hash_algo` so the Rust launcher uses the matching hash. BLAKE3 is
preferred (much faster on 8 GB, multithreaded); if the `blake3` module isn't installed we
fall back to stdlib BLAKE2b-256. `pip install blake3` to speed publishes up.
"""

import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
import re
import struct
import time
import zipfile


def utcnow():
    return datetime.datetime.now(datetime.timezone.utc)

# ---- hashing ---------------------------------------------------------------

try:
    import blake3 as _blake3
    HASH_ALGO = "blake3"
except Exception:
    _blake3 = None
    HASH_ALGO = "blake2b-256"

_CHUNK = 4 * 1024 * 1024


def hash_file(path):
    if _blake3 is not None:
        h = _blake3.blake3(max_threads=_blake3.blake3.AUTO)
    else:
        h = hashlib.blake2b(digest_size=32)
    with open(path, "rb", buffering=0) as f:
        while True:
            b = f.read(_CHUNK)
            if not b:
                break
            h.update(b)
    return h.hexdigest()


# ---- ship / skip rules -----------------------------------------------------
# Rules are intentionally conservative: we EXCLUDE dev/source/backup and everything
# else ships. Audit included.txt / excluded.txt after every run.

# gamedir-relative directory names skipped anywhere in the tree
SKIP_DIRS = {
    "src", "tools", "_prerender_backup", "screenshots", "dlcache",
    "_staging", ".git", "__pycache__", ".vs", "launcher",
    # textures/disable/ holds ~455 *_norm.png maps that were switched off by being moved
    # here: FTE resolves a normal map as textures/<name>_norm, so nothing under a
    # disable/ subdir is reachable by any material. Shipping them is 141 MB of dead weight.
    "disable",
}

# extensions never shipped (source meshes, build cache, dev/debug, editor sources)
SKIP_EXT = {
    ".obj", ".iqe", ".acd", ".smd", ".mtl", ".cmd",      # source-mesh + build cache
    ".log", ".lno", ".db", ".pdb",                        # logs / debug symbols
    ".pfx", ".bak", ".tmp", ".orig",                      # secrets / scratch
    ".blend", ".blend1", ".psd", ".xcf", ".ztl",          # editor sources
    ".py", ".pyc", ".ps1", ".sh",                         # tooling (belt-and-suspenders)
}

# OS/file-manager droppings that get created invisibly and are pure noise in a game install
SKIP_ANY_NAME = {"desktop.ini", "thumbs.db", ".ds_store"}

# exact filenames skipped at the gamedir root
SKIP_ROOT_FILES = {
    "csqccore.txt", "ssqccore.txt", "crashaddr.txt",
    "installed.lst", "identity.pfx", "qconsole.log",
}

# gamedir-root filename PREFIXES that are dev scratch. conhistory.txt is the engine's
# console-input history and Windows makes copies of it ("conhistory (2).txt"), so an
# exact-name list never keeps up.
SKIP_ROOT_PREFIXES = ("conhistory", "qconsole")

# Engine binaries (component = "engine"). Entries are (filename, source-subdir-of-install-root,
# needs-exec-bit). The ~6.3 GB of game content is identical on every platform and is tagged
# platform "all"; only this ~15 MB set differs, and the content-addressed object tree means the
# shared blobs are stored and downloaded exactly once regardless of how many platforms ship.
#
# Linux binaries are produced in WSL (see deploy/linux-build.md) and staged into
# C:\FTEQuake\_engine\linux64 by that script.
SHARED_ENGINE_FILES = [
    ("default.fmf", ".", False),
]

PLATFORM_ENGINE_FILES = {
    "win64": [
        ("fteqw64.exe", ".", True),
        ("sqlite3.dll", ".", False),
        ("fteplug_ode_x64.dll", ".", False),
        ("fteplug_box3d_x64.dll", ".", False),
        ("fteplug_hl2_x64.dll", ".", False),
        ("fteplug_cod_x64.dll", ".", False),
    ],
    # quakers/default.cfg does `plug_load box3d / hl2 / cod`, so those three .so files are
    # not optional — box3d is the physics backend behind every prop and ragdoll.
    # No sqlite3 shim: FTE dlopen()s the system libsqlite3.so.0 on Linux.
    "linux64": [
        ("fteqw-gl64", "_engine/linux64", True),
        ("fteqw-sv64", "_engine/linux64", True),
        ("fteplug_box3d_amd64.so", "_engine/linux64", False),
        ("fteplug_hl2_amd64.so", "_engine/linux64", False),
        ("fteplug_cod_amd64.so", "_engine/linux64", False),
    ],
}

# How the launcher starts the game on each platform.
EXEC_BY_PLATFORM = {
    "win64": {"cmd": "fteqw64.exe", "args": ["-manifest", "default.fmf"]},
    "linux64": {"cmd": "fteqw-gl64", "args": ["-manifest", "default.fmf"]},
}


# image extensions considered when matching loose textures against pk3 contents
IMG_EXT = {".png", ".dds", ".tga", ".jpg", ".jpeg", ".ktx", ".vtf", ".bmp", ".lmp", ".pcx"}


def _stem_forms(relslash):
    """Extensionless path forms to match a texture on, with/without a leading textures/."""
    base = os.path.splitext(relslash.lower())[0]
    forms = {base}
    if base.startswith("textures/"):
        forms.add(base[len("textures/"):])
    else:
        forms.add("textures/" + base)
    return forms


def build_pk3_texture_index(gamedir):
    """Set of extensionless image-path forms found inside every *.pk3 in the gamedir.
    Used to drop loose textures/ files that are already shipped (compressed) in a pk3."""
    stems = set()
    for name in sorted(os.listdir(gamedir)):
        if not name.lower().endswith(".pk3"):
            continue
        path = os.path.join(gamedir, name)
        try:
            with zipfile.ZipFile(path) as zf:
                for info in zf.infolist():
                    if info.is_dir():
                        continue
                    if os.path.splitext(info.filename)[1].lower() in IMG_EXT:
                        stems |= _stem_forms(info.filename.replace("\\", "/"))
        except zipfile.BadZipFile:
            pass
    return stems


def map_referenced_textures(gamedir):
    """Texture basenames the shipped content actually asks for.

    The loose textures/ tree is a map-editor palette (AmbientCG-style sets used from
    TrenchBroom), so most of it belongs to maps that are still in development rather
    than to anything being shipped. This collects what the shipped content references:

      * Q1 / HL BSP  -- miptex names in the texture lump
      * Source VBSP  -- the texdata string table
      * loose text assets (.skin/.shader/.mat/.txt/.cfg) -- any token that looks like a
        texture path, so model skins and material scripts keep their textures

    Returned names are lowercase, extensionless basenames.
    """
    refs = set()

    def q1_miptex(path, data):
        ver = struct.unpack_from("<i", data, 0)[0]
        if ver not in (29, 30):
            return
        off, _ln = struct.unpack_from("<ii", data, 4 + 2 * 8)   # lump 2 = textures
        n = struct.unpack_from("<i", data, off)[0]
        if not (0 < n < 65536):
            return
        for i in range(n):
            d = struct.unpack_from("<i", data, off + 4 + 4 * i)[0]
            if d < 0:
                continue
            nm = data[off + d:off + d + 16].split(b"\0")[0].decode("latin-1").lower()
            if nm:
                refs.add(nm)

    def vbsp_texdata(path, data):
        # Source BSP: lump 43 is LUMP_TEXDATA_STRING_DATA, a run of NUL-terminated names.
        off, ln = struct.unpack_from("<ii", data, 8 + 43 * 16)
        if not (0 < ln < 4 * 1024 * 1024) or off + ln > len(data):
            return
        for tok in data[off:off + ln].split(b"\0"):
            if tok:
                refs.add(os.path.basename(tok.decode("latin-1").lower().replace("\\", "/")))

    mapdir = os.path.join(gamedir, "maps")
    if os.path.isdir(mapdir):
        for fn in os.listdir(mapdir):
            if not fn.lower().endswith(".bsp"):
                continue
            p = os.path.join(mapdir, fn)
            try:
                with open(p, "rb") as f:
                    data = f.read()
                if data[:4] in (b"VBSP", b"RBSP", b"IBSP", b"FBSP"):
                    vbsp_texdata(p, data)
                else:
                    q1_miptex(p, data)
            except Exception:
                # an unparseable map must never silently drop its textures
                raise

    # model skins / material scripts / cfgs naming textures directly
    TEXTREF_EXT = {".skin", ".shader", ".mat", ".txt", ".cfg", ".particles", ".framegroups"}
    for dirpath, dirnames, filenames in os.walk(gamedir):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for fn in filenames:
            if os.path.splitext(fn)[1].lower() not in TEXTREF_EXT:
                continue
            try:
                with open(os.path.join(dirpath, fn), "r", encoding="utf-8", errors="ignore") as f:
                    body = f.read(1 << 20)
            except OSError:
                continue
            for tok in re.findall(r"[A-Za-z0-9_{}!+#\-/\\.]{3,128}", body):
                tok = tok.lower().replace("\\", "/")
                refs.add(os.path.splitext(os.path.basename(tok))[0])

    # Quake names liquid textures `*water1`, but `*` is illegal in a filename, so the
    # on-disk replacement is `!water1.png`. Match both spellings or a trim would delete
    # every loose water/lava/teleport texture while the map still asks for it.
    for n in list(refs):
        if n.startswith("*"):
            refs.add("!" + n[1:])
        elif n.startswith("!"):
            refs.add("*" + n[1:])

    refs.discard("")
    return refs


def skip_reason(rel, name, ext, pk3_tex=frozenset(), used_tex=None):
    """Return a short reason string if this gamedir-relative file should be skipped, else None."""
    low = name.lower()
    relslash = rel.replace("\\", "/")
    # split-off dir prefix (top component)
    top = relslash.split("/", 1)[0] if "/" in relslash else ""

    if low in SKIP_ANY_NAME:
        return "os-junk"
    if ext in SKIP_EXT:
        return "ext"
    if "/" not in relslash and low in SKIP_ROOT_FILES:
        return "root-junk"
    if "/" not in relslash and low.startswith(SKIP_ROOT_PREFIXES):
        return "root-junk"
    # editor-only prop PNGs (game reads the BC7 .dds inside the pk3s); world textures under
    # textures/ and HUD art under gfx/ are kept.
    if ext == ".png" and top == "models":
        return "editor-png"
    # loose world textures whose image already lives inside a pk3 (game loads the pk3 copy);
    # loose textures NOT in any pk3 are kept (they'd otherwise vanish from maps).
    if top == "textures" and ext in IMG_EXT and (_stem_forms(relslash) & pk3_tex):
        return "tex-in-pk3"
    # loose editor-palette textures no shipped map/skin/material actually references
    # (opt-in: --trim-unused-textures)
    if used_tex is not None and top == "textures" and ext in IMG_EXT:
        if os.path.splitext(os.path.basename(relslash))[0] not in used_tex:
            return "tex-unused"
    # loose screenshot PNGs dumped at the gamedir root
    if ext == ".png" and "/" not in relslash:
        return "root-screenshot"
    # loose .dds at root of models packs left over from packaging (they live in the pk3s now)
    if low.endswith(".exe") and ("_new" in low or "_prev" in low):
        return "staged-exe"
    if low.endswith(".prev"):
        return "prev"
    return None


# ---- walk ------------------------------------------------------------------

def collect(install_root, gamedir, gamedir_name, platforms, used_tex=None):
    """Return (entries, skipped).

    entries = list of (abspath, manifest_path, component, platform, needs_exec_bit).
    """
    entries = []
    skipped = []

    # engine binaries from the install root -> manifest path is just the filename,
    # so a tester ends up with the engine sitting next to the gamedir.
    def add_engine(name, subdir, is_exec, platform):
        p = os.path.normpath(os.path.join(install_root, subdir, name))
        if os.path.isfile(p):
            entries.append((p, name, "engine", platform, is_exec))
        else:
            skipped.append((f"{platform}:{name}", "MISSING-engine-file"))

    for name, subdir, is_exec in SHARED_ENGINE_FILES:
        add_engine(name, subdir, is_exec, "all")
    for plat in platforms:
        for name, subdir, is_exec in PLATFORM_ENGINE_FILES.get(plat, []):
            add_engine(name, subdir, is_exec, plat)

    # index pk3 textures once so we can drop loose textures/ files already in a pk3
    pk3_tex = build_pk3_texture_index(gamedir)

    # gamedir tree -> manifest path is "<gamedir_name>/<rel>"
    for dirpath, dirnames, filenames in os.walk(gamedir):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for fn in filenames:
            ap = os.path.join(dirpath, fn)
            rel = os.path.relpath(ap, gamedir)
            ext = os.path.splitext(fn)[1].lower()
            reason = skip_reason(rel, fn, ext, pk3_tex, used_tex)
            if reason:
                skipped.append((rel, reason))
                continue
            mpath = (gamedir_name + "/" + rel.replace("\\", "/"))
            entries.append((ap, mpath, "game", "all", False))

    return entries, skipped


# ---- hash cache ------------------------------------------------------------

def load_cache(path):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}


def save_cache(path, cache):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(cache, f)
    os.replace(tmp, path)


def human(n):
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:.1f} {unit}" if unit != "B" else f"{n} B"
        n /= 1024.0


# ---- integrity -------------------------------------------------------------

def verify_objects(out, jobs):
    """Re-hash every blob in objects/ and confirm it still matches its filename.

    Objects are hardlinked to their source files, so anything that rewrites a source
    IN PLACE (a plain `cp`, an editor saving over it, a rebuild) also rewrites the
    published blob while leaving its hash-derived name untouched. Uploading that ships
    a file that fails verification on every client. Cheap insurance before a push.
    """
    objroot = os.path.join(out, "objects")
    if not os.path.isdir(objroot):
        print(f"no object tree at {objroot}")
        return 1

    blobs = []
    for sub in sorted(os.listdir(objroot)):
        d = os.path.join(objroot, sub)
        if os.path.isdir(d):
            for name in os.listdir(d):
                blobs.append((os.path.join(d, name), name))

    print(f"verifying {len(blobs)} object(s) in {objroot} with {HASH_ALGO} ...")
    bad = []
    done = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as ex:
        for path, name, got in ex.map(
            lambda b: (b[0], b[1], hash_file(b[0])), blobs
        ):
            done += 1
            if got != name:
                bad.append((path, name, got))
            if done % 500 == 0 or done == len(blobs):
                print(f"  {done}/{len(blobs)}", end="\r", flush=True)
    print()

    if bad:
        print(f"\nCORRUPT: {len(bad)} object(s) do not match their own name:")
        for path, name, got in bad[:20]:
            print(f"  {name[:16]}… -> actually {got[:16]}…   {path}")
        if len(bad) > 20:
            print(f"  … and {len(bad) - 20} more")
        print("\nA source file was almost certainly overwritten in place, mutating the\n"
              "hardlinked blob. Re-run `python publish.py --prune` to rebuild the tree,\n"
              "and use `cp --remove-destination` when staging binaries.")
        return 1

    print(f"OK: all {len(blobs)} objects match their hashes.")
    return 0


# ---- main ------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="Build the quakers distribution manifest + object tree.")
    ap.add_argument("--install-root", default=r"C:\FTEQuake")
    ap.add_argument("--gamedir", default=r"C:\FTEQuake\quakers")
    ap.add_argument("--out", default=r"C:\FTEQuake\launcher\dist")
    # Player-facing: the launcher header prints this verbatim ("Release alpha - <version>").
    # Keep in step with the --channel default in launcher/src/main.rs.
    ap.add_argument("--channel", default="alpha")
    ap.add_argument("--version", default=None, help="build id (default: UTC timestamp)")
    # Keep in step with `version` in Cargo.toml -- this is what a future self-update check
    # would compare against, so a stale value here would tell every client it is current.
    ap.add_argument("--launcher-version", default="0.1.2")
    ap.add_argument("--mirrors", nargs="*", default=["https://dl.proto.bar"])
    ap.add_argument("--objects", dest="objects", action="store_true", default=True,
                    help="build the content-addressed objects/ tree (default)")
    ap.add_argument("--no-objects", dest="objects", action="store_false")
    ap.add_argument("--link", choices=["hardlink", "copy"], default="hardlink",
                    help="how to populate objects/ (hardlink is free; copy if crossing volumes)")
    ap.add_argument("--stage-dir", default=None,
                    help="also materialize the clean install as a real-named, BROWSABLE tree here "
                         "(exactly what a tester ends up with) -- separate from the hash-named objects/")
    ap.add_argument("--prune", action="store_true",
                    help="delete objects/ blobs not referenced by this manifest (removes orphans "
                         "left by prior, larger builds)")
    ap.add_argument("--report-only", action="store_true",
                    help="classify + size only; no hashing, no manifest, no objects")
    ap.add_argument("--jobs", type=int, default=min(8, (os.cpu_count() or 4)))
    ap.add_argument("--trim-unused-textures", action="store_true",
                    help="drop loose textures/ images that no shipped map, model skin or "
                         "material script references. The loose tree is a map-EDITOR palette, "
                         "so most of it belongs to maps still in development. Opt-in: verify "
                         "the excluded.txt 'tex-unused' list before shipping with this on.")
    ap.add_argument("--verify-objects", action="store_true",
                    help="re-hash every blob in objects/ and check it still matches its own "
                         "filename, then exit. Objects are HARDLINKED to their sources, so "
                         "overwriting a source file in place (cp without --remove-destination) "
                         "silently rewrites an already-published blob. Run this before uploading.")
    ap.add_argument("--allow-hash-change", action="store_true",
                    help="permit publishing with a different hash_algo than the previous build of "
                         "this channel (renames EVERY object -> full re-upload and re-download)")
    ap.add_argument("--platforms", nargs="*", default=list(PLATFORM_ENGINE_FILES),
                    choices=list(PLATFORM_ENGINE_FILES),
                    help="which platforms' engine binaries to include (default: all of them). "
                         "The game content is shared, so adding a platform costs only its engine set.")
    args = ap.parse_args()

    if args.verify_objects:
        raise SystemExit(verify_objects(args.out, args.jobs))

    platforms = list(dict.fromkeys(args.platforms))
    gamedir_name = os.path.basename(os.path.normpath(args.gamedir))
    version = args.version or utcnow().strftime("%Y.%m.%d_%H%M")

    # Object names ARE content hashes, so changing the algorithm renames every blob: a full
    # re-upload of the whole payload and a full re-download for every tester. That is far too
    # easy to trigger by accident -- merely `pip install blake3` flips HASH_ALGO on the next run.
    prev_path = os.path.join(args.out, "manifests", f"{args.channel}.json")
    if os.path.isfile(prev_path) and not args.allow_hash_change:
        try:
            with open(prev_path, "r", encoding="utf-8") as f:
                prev_algo = json.load(f).get("hash_algo")
        except Exception:
            prev_algo = None
        if prev_algo and prev_algo != HASH_ALGO:
            print(f"REFUSING TO PUBLISH: channel '{args.channel}' was last built with "
                  f"'{prev_algo}', but this run would use '{HASH_ALGO}'.\n"
                  f"  Every object would be renamed -> full re-upload + full re-download "
                  f"for every tester.\n"
                  f"  Either restore the '{prev_algo}' setup (e.g. `pip uninstall blake3`), "
                  f"or re-run with --allow-hash-change if you really mean it.")
            raise SystemExit(2)

    print(f"install-root : {args.install_root}")
    print(f"gamedir      : {args.gamedir}  (mounts as '{gamedir_name}/')")
    print(f"platforms    : {', '.join(platforms)}")
    print(f"hash algo    : {HASH_ALGO}" + ("" if _blake3 else "   (pip install blake3 for faster publishes)"))
    print(f"channel/ver  : {args.channel} / {version}\n")

    t0 = time.time()
    used_tex = None
    if args.trim_unused_textures:
        used_tex = map_referenced_textures(args.gamedir)
        print(f"texture refs : {len(used_tex)} names referenced by shipped maps/skins/materials")
    entries, skipped = collect(args.install_root, args.gamedir, gamedir_name, platforms, used_tex)

    # size classification report
    by_top = {}
    by_platform = {}
    total_bytes = 0
    for ap_, mpath, comp, plat, _x in entries:
        try:
            sz = os.path.getsize(ap_)
        except OSError:
            sz = 0
        total_bytes += sz
        top = mpath.split("/", 1)[0] if comp == "engine" else mpath.split("/")[1] if "/" in mpath[len(gamedir_name)+1:] else "(root)"
        key = "engine" if comp == "engine" else top
        b = by_top.get(key, [0, 0]); b[0] += 1; b[1] += sz; by_top[key] = b
        p = by_platform.get(plat, [0, 0]); p[0] += 1; p[1] += sz; by_platform[plat] = p

    skip_bytes = 0
    skip_by_reason = {}
    for rel, reason in skipped:
        try:
            skip_bytes += os.path.getsize(os.path.join(args.gamedir, rel))
        except OSError:
            pass
        skip_by_reason[reason] = skip_by_reason.get(reason, 0) + 1

    print(f"SHIP  : {len(entries):>6} files  {human(total_bytes)}")
    for k in sorted(by_top, key=lambda k: -by_top[k][1]):
        c, b = by_top[k]
        print(f"        {k:<14} {c:>6} files  {human(b)}")
    print(f"SKIP  : {len(skipped):>6} files  {human(skip_bytes)}   ({dict(sorted(skip_by_reason.items()))})")
    for k in sorted(by_platform):
        c, b = by_platform[k]
        label = "shared by all platforms" if k == "all" else f"{k}-only"
        print(f"        {k:<8} {c:>6} files  {human(b):>10}   ({label})")
    print()

    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "included.txt"), "w", encoding="utf-8") as f:
        for _, mpath, comp, plat, _x in sorted(entries, key=lambda e: e[1]):
            f.write(f"{comp}\t{plat}\t{mpath}\n")
    with open(os.path.join(args.out, "excluded.txt"), "w", encoding="utf-8") as f:
        for rel, reason in sorted(skipped):
            f.write(f"{reason}\t{rel}\n")

    # optional: materialize the clean install as a real-named tree -- the "default install" you can browse
    if args.stage_dir:
        import shutil
        os.makedirs(args.stage_dir, exist_ok=True)
        staged = 0
        for ap_, mpath, comp, _plat, _x in entries:
            tgt = os.path.join(args.stage_dir, mpath.replace("/", os.sep))
            os.makedirs(os.path.dirname(tgt), exist_ok=True)
            if os.path.exists(tgt):
                try:
                    os.remove(tgt)
                except OSError:
                    pass
            try:
                if args.link == "hardlink":
                    os.link(ap_, tgt)
                else:
                    shutil.copyfile(ap_, tgt)
            except OSError:
                shutil.copyfile(ap_, tgt)   # cross-volume -> copy
            staged += 1
        print(f"staged clean install tree: {staged} files -> {args.stage_dir}\n")

    if args.report_only:
        print(f"report-only: wrote included.txt / excluded.txt to {args.out}  ({time.time()-t0:.1f}s)")
        return

    # ---- hash (with mtime/size cache) --------------------------------------
    cache_path = os.path.join(args.out, ".hashcache.json")
    cache = load_cache(cache_path)
    new_cache = {}

    def do_one(item):
        ap_, mpath, comp, plat, is_exec = item
        st = os.stat(ap_)
        key = ap_
        ce = cache.get(key)
        if ce and ce.get("mtime") == st.st_mtime_ns and ce.get("size") == st.st_size and ce.get("algo") == HASH_ALGO:
            digest = ce["hash"]
        else:
            digest = hash_file(ap_)
        return (ap_, mpath, comp, st.st_size, st.st_mtime_ns, digest, plat, is_exec)

    print(f"hashing {len(entries)} files with {args.jobs} workers ...")
    results = []
    done = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as ex:
        for r in ex.map(do_one, entries):
            results.append(r)
            new_cache[r[0]] = {"mtime": r[4], "size": r[3], "hash": r[5], "algo": HASH_ALGO}
            done += 1
            if done % 500 == 0 or done == len(entries):
                print(f"  {done}/{len(entries)}", end="\r", flush=True)
    print()
    save_cache(cache_path, new_cache)

    # ---- objects/ tree -----------------------------------------------------
    objroot = os.path.join(args.out, "objects")
    n_new = 0
    if args.objects:
        for ap_, mpath, comp, sz, mt, digest, _plat, _x in results:
            odir = os.path.join(objroot, digest[:2])
            opath = os.path.join(odir, digest)
            if os.path.exists(opath):
                continue
            os.makedirs(odir, exist_ok=True)
            try:
                if args.link == "hardlink":
                    os.link(ap_, opath)
                else:
                    import shutil
                    shutil.copyfile(ap_, opath)
                n_new += 1
            except OSError:
                import shutil
                shutil.copyfile(ap_, opath)   # hardlink failed (cross-volume) -> copy
                n_new += 1

    if args.prune and args.objects:
        referenced = {r[5] for r in results}
        removed = 0
        for sub in os.listdir(objroot):
            subdir = os.path.join(objroot, sub)
            if not os.path.isdir(subdir):
                continue
            for blob in os.listdir(subdir):
                if blob not in referenced:
                    try:
                        os.remove(os.path.join(subdir, blob))
                        removed += 1
                    except OSError:
                        pass
            try:
                if not os.listdir(subdir):
                    os.rmdir(subdir)
            except OSError:
                pass
        print(f"pruned {removed} orphan blob(s) from objects/")

    # ---- manifest ----------------------------------------------------------
    # `platform` is omitted for shared content so the manifest doesn't grow by ~5000
    # redundant "all" strings; the launcher defaults a missing platform to "all".
    files = []
    for (ap_, mpath, comp, sz, mt, digest, plat, is_exec) in sorted(results, key=lambda r: r[1]):
        e = {"path": mpath, "size": sz, "hash": digest, "component": comp}
        if plat != "all":
            e["platform"] = plat
        if is_exec:
            e["exec"] = True
        files.append(e)

    manifest = {
        "schema": 2,
        "channel": args.channel,
        "version": version,
        "created_utc": utcnow().strftime("%Y-%m-%dT%H:%M:%SZ"),
        "hash_algo": HASH_ALGO,
        "launcher_version": args.launcher_version,
        "install_root_name": os.path.basename(os.path.normpath(args.install_root)),
        # schema-1 readers only understand this one; keep it pointing at Windows.
        "exec": EXEC_BY_PLATFORM["win64"],
        "exec_by_platform": {p: EXEC_BY_PLATFORM[p] for p in platforms if p in EXEC_BY_PLATFORM},
        "mirrors": args.mirrors,
        "object_layout": "objects/{h0h1}/{hash}",
        "platforms": platforms,
        "total_files": len(files),
        "total_bytes": total_bytes,
        "files": files,
    }
    man_dir = os.path.join(args.out, "manifests")
    os.makedirs(man_dir, exist_ok=True)
    man_path = os.path.join(man_dir, f"{args.channel}.json")
    tmp = man_path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=1)
    os.replace(tmp, man_path)

    dt = time.time() - t0
    print(f"\nmanifest : {man_path}  ({len(files)} files, {human(total_bytes)})")
    if args.objects:
        print(f"objects  : {objroot}  (+{n_new} new blobs this run, {args.link})")
    print(f"done in {dt:.1f}s\n")
    print("next: sign the manifest, then rclone sync objects/ + copy manifests/ to R2 and the Pi")
    print("      (see C:\\FTEQuake\\launcher\\deploy\\rclone-and-r2-setup.md)")


if __name__ == "__main__":
    main()
