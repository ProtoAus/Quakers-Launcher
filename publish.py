#!/usr/bin/env python3
"""
publish.py -- build a quakers player-install manifest + content-addressed object tree
for distribution via Cloudflare R2 (bucket `quakers-dl`, served at dl.proto.bar).

It walks the engine install root and the quakers gamedir, applies the ship/skip rules,
hashes every shippable file, and writes:

    <out>/manifests/<channel>.json      the manifest (path / size / hash / component)
    <out>/objects/<hh>/<hash>           content-addressed blobs (hardlinked from source)
    <out>/included.txt, excluded.txt    audit lists

Nothing is uploaded here. Push with:
    .\deploy\r2-push.ps1

which sends objects first and the manifest LAST -- the manifest is the atomic switch that
makes a release live, so publishing it before its blobs exist hands testers a manifest that
references objects nobody can download. The script also sets the per-prefix Cache-Control and
forces IPv4; see deploy/rclone-and-r2-setup.md for why both matter.

The proto.bar Pi is no longer a mirror. It was one until 2026-07-27, when a single tester's
install saturated the home uplink: Cloudflare's cache is per-edge-server and the launcher runs
8 parallel workers, so the origin served 6.20 GB to deliver a 5.84 GB payload.

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
import sys
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

# Directory names that are only dev scratch AT THE GAMEDIR ROOT. They must NOT be matched deeper:
# "tools" was in the any-depth set below until 2026-07-28, which silently dropped
# models/props/tools/ -- 336 files of real prop art, including 11 props 2fort places -- from
# every release. The models resolved fine locally, so it only showed up as missing models in game.
SKIP_DIRS_ROOT = {"tools", "launcher"}

# gamedir-relative directory names skipped anywhere in the tree
SKIP_DIRS = {
    "src", "_prerender_backup", "screenshots", "dlcache",
    "_staging", ".git", "__pycache__", ".vs",
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
    # External coloured-lighting files. Every shipped map has its lighting baked into the .bsp,
    # so these are ~32 MB of duplicate lightmap data the engine loads over identical baked data.
    # If a future map genuinely needs an external .lit, drop this and skip per-map instead.
    ".lit",
}

# OS/file-manager droppings that get created invisibly and are pure noise in a game install
SKIP_ANY_NAME = {"desktop.ini", "thumbs.db", ".ds_store"}

# exact filenames skipped at the gamedir root
SKIP_ROOT_FILES = {
    "csqccore.txt", "ssqccore.txt", "crashaddr.txt",
    "installed.lst", "identity.pfx", "qconsole.log",
    # Engine-generated gib/impact filter lists. Both are 0 bytes here and nothing in the
    # mod reads or writes them -- FTE recreates them on demand. Shipping an empty file is
    # a download, a manifest entry and a disk write to deliver no content.
    "gibfiltr.cfg", "impfiltr.cfg",
}

# gamedir-root filename PREFIXES that are dev scratch. conhistory.txt is the engine's
# console-input history and Windows makes copies of it ("conhistory (2).txt"), so an
# exact-name list never keeps up.
SKIP_ROOT_PREFIXES = ("conhistory", "qconsole")

# Directory names skipped wherever they appear in the path. maps/_loosepack_bsp_backup/ held
# five older copies of shipped .bsp files and was publishing all of them.
SKIP_DIR_NAMES = {"_loosepack_bsp_backup"}

# Map basenames we are allowed to ship, loaded from the gamedir's cfg/maps.txt -- the SAME file
# m_main.qc reads to pick the backdrop map and m_createserver.qc reads for its map pool. Deriving
# it rather than hardcoding a list here means editing that one file changes both what the game
# offers and what the release contains, so they cannot drift apart.
MAPS_ALLOWED = set()


def load_maps_allowlist(gamedir):
    """Read cfg/maps.txt into MAPS_ALLOWED. Refuses to publish if it is missing or empty.

    Failing open (shipping every map) would silently undo the restriction and put ~560 MB of
    dev maps back in the release; failing closed without saying so would ship a game with no
    maps at all. Both are worse than stopping.
    """
    p = os.path.join(gamedir, "cfg", "maps.txt")
    if not os.path.isfile(p):
        raise SystemExit(f"REFUSING TO PUBLISH: {p} not found -- it decides which maps ship.")
    with open(p, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("//"):
                MAPS_ALLOWED.add(os.path.splitext(line)[0].lower())
    if not MAPS_ALLOWED:
        raise SystemExit(f"REFUSING TO PUBLISH: {p} lists no maps.")
    return MAPS_ALLOWED


# Durable record of every path any past release has retired. Kept OUTSIDE the manifest because
# the manifest is rewritten by every publish: building twice without pushing (or discarding a
# build) would otherwise erase the history and quietly stop telling testers to delete anything.
RETIRED_STORE = "retired.json"


def load_retired(out_dir):
    try:
        with open(os.path.join(out_dir, RETIRED_STORE), "r", encoding="utf-8") as f:
            return set(json.load(f))
    except Exception:
        return set()


def save_retired(out_dir, paths):
    os.makedirs(out_dir, exist_ok=True)
    p = os.path.join(out_dir, RETIRED_STORE)
    tmp = p + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(sorted(paths), f, indent=1)
    os.replace(tmp, p)


def prune_dirs(dirpath, dirnames, gamedir):
    """In-place os.walk pruning: drop scratch dirs, root-only names only at the root."""
    at_root = os.path.normpath(dirpath) == os.path.normpath(gamedir)
    dirnames[:] = [d for d in dirnames
                   if d not in SKIP_DIRS and not (at_root and d in SKIP_DIRS_ROOT)]


def derive_removed(prev_path, new_paths, platforms):
    """Everything an installed copy must delete, CUMULATIVELY. Returns (all, newly_dropped).

    Hand-maintaining this is the thing that rots. Every repack that folds loose files into a pk3,
    every renamed directory, every trimmed map drops dozens of paths at once, and anything missed
    sits on every tester's disk forever. Diffing the previous manifest catches all of it for free.

    It MUST accumulate, not just diff. A tester can be several releases behind, and if release N+1
    only lists what release N dropped, someone updating from N-1 straight to N+1 never hears about
    N's retirements and keeps them forever. So carry the previous manifest's `removed` forward and
    add this run's drops to it.

    Two guards:
      * anything present in the new manifest is subtracted, so a path that is retired and later
        re-added does not get deleted right after being downloaded.
      * a file is only newly retired if its platform is one we actually built this run. Without
        that, `--platforms win64` would drop every linux64 entry from the manifest and the diff
        would cheerfully tell Linux testers to delete their engine.
    """
    try:
        with open(prev_path, "r", encoding="utf-8") as f:
            prev = json.load(f)
    except Exception:
        return set(), []
    want = set(platforms) | {"all"}
    dropped = [e["path"] for e in prev.get("files", [])
               if e.get("path") and e["path"] not in new_paths
               and e.get("platform", "all") in want]
    carried = set(prev.get("removed", []))
    return (carried | set(dropped)) - new_paths, sorted(dropped)


def map_stem(relslash):
    """Map name a file under maps/ belongs to.

    Split at the FIRST dot, not the last: companions stack suffixes on the full map name
    (notnormals.bsp.json, notnormals_shadowtest.bsp.lm_0.png, 2fort.lit, notnormals.pts).
    Map names contain no dots, so the first-dot split recovers the map for every one of them.
    """
    return os.path.basename(relslash).split(".", 1)[0].lower()

# Gamedir-relative paths (forward slashes, lowercase) that are per-machine RUNTIME STATE, not
# content. These live in subdirectories, so SKIP_ROOT_FILES -- which only matches at the gamedir
# root -- never sees them, and they quietly shipped for months. Each is rewritten by simply
# playing the game, so every publish pushed one machine's state to every tester.
#
# These paths moved from data/ to cfg/ on 2026-07-28. If you ever rename that directory again,
# CHANGE THEM HERE IN THE SAME COMMIT: this list is matched on the exact relative path, so a
# stale entry does not error -- it silently stops skipping, and the next publish pushes one
# machine's keybinds and map cache to every tester. That is the quietest failure in this script,
# which is why the summary asserts the expected number of runtime-state skips actually fired.
SKIP_RELPATHS = {
    # What the settings menu writes when you hit save: keybinds, sensitivity, and the
    # resolution/audio-device choices, which suit exactly one machine.
    "cfg/settings.cfg",
    # Engine-generated map cache. Regenerated on demand, and it changes whenever the local
    # map set does -- so it is noise in the manifest and wrong for anyone else.
    "cfg/maps_index.txt",
    # `sv_writecvars` dumps every cvar here as a reference. Regenerated on demand, never exec'd
    # by anything, and 25 KB of pure noise in the manifest.
    "cfg/allcommands.cfg",
    # Local stats database. Pure runtime state; shipping it hands every tester our numbers.
    "sqlite/quakers_stats.d",
    # Same, under the mod's former name. sv_progression.qc does sqlconnect(..."quakers_stats"...),
    # so nothing has written this since the rename -- it is a stale local DB that we were
    # nevertheless publishing to everyone.
    "sqlite/nettest_stats.d",
}

# Paths to DELETE from an existing install, emitted as the manifest's top-level "removed" array.
#
# Nothing else prunes: both the launcher and the in-game updater only ever add or overwrite, so a
# file that leaves the manifest lingers on every tester's disk forever. Without this the cfg/
# move would leave a stale data/ folder holding a second, older copy of every config -- exactly
# the split this change exists to end.
#
# Safe to leave entries here indefinitely: deleting an already-absent file is a no-op. Both
# consumers path-validate before unlinking, so nothing here can escape the install root.
REMOVED_PATHS = [
    # the data/ -> cfg/ move (2026-07-28)
    "quakers/data/default.cfg",
    "quakers/data/server.cfg",
    "quakers/data/ftesrv.cfg",
    "quakers/data/allcommands.cfg",
    "quakers/data/engine_tweaks.cfg",
    "quakers/data/maps.txt",
    "quakers/data/settings.cfg",
    "quakers/data/maps_index.txt",
    "quakers/data/scripts/sprays.shader",
    # boot configs that now live in cfg/ -- if these survive, the engine still prefers cfg/ but
    # the duplicate copies are precisely the confusion being removed.
    "quakers/default.cfg",
    "quakers/server.cfg",
    "quakers/ftesrv.cfg",
    "quakers/backupcommands.cfg",
    # runtime state that shipped by accident before SKIP_RELPATHS existed, and is therefore
    # still sitting in every install made before 2026-07-27.
    "quakers/sqlite/quakers_stats.d",
    "quakers/sqlite/nettest_stats.d",
    # cubemap/ -> gfx/cubemap/ (2026-07-28). The flashlight cookie's only consumer is
    # FLASHLIGHT_COOKIE_SHADER in cl_flashlight.qc, moved to match in the same change.
    "quakers/cubemap/flashlight.png",
    "quakers/cubemap/flashlight_nx.tga",
    "quakers/cubemap/flashlight_ny.tga",
    "quakers/cubemap/flashlight_nz.tga",
    "quakers/cubemap/flashlight_px.tga",
    "quakers/cubemap/flashlight_px_fullres.tga",
    "quakers/cubemap/flashlight_py.tga",
    "quakers/cubemap/flashlight_pz.tga",
]

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
    # ODE is deliberately NOT shipped. Box3D is the physics backend for every prop, ragdoll
    # and vehicle (sv_physics_engine defaults to box3d and quakers/default.cfg plug_loads only
    # box3d at boot), so the ODE plugin was dead weight in the payload. `sv_physics_engine ode`
    # therefore only works on a dev machine that still has fteplug_ode_* beside the exe --
    # see the guard comment in server/sv_main.qc.
    "win64": [
        ("fteqw64.exe", ".", True),
        ("sqlite3.dll", ".", False),
        ("fteplug_box3d_x64.dll", ".", False),
        ("fteplug_hl2_x64.dll", ".", False),
        ("fteplug_cod_x64.dll", ".", False),
    ],
    # quakers/default.cfg does `plug_load box3d / hl2 / cod`, so those three .so files are
    # not optional -- box3d is the physics backend behind every prop and ragdoll. If you drop
    # hl2 or cod from a platform here, flip the matching fs_have_* flag in quakers/cfg/default.cfg
    # (and ftesrv.cfg) to 0 so the Create Server menu stops indexing and mounting maps that the
    # engine can no longer parse.
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
        prune_dirs(dirpath, dirnames, gamedir)
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
    if SKIP_DIR_NAMES.intersection(c.lower() for c in relslash.split("/")[:-1]):
        return "backup-dir"
    # Maps not listed in cfg/maps.txt, and their companions (.lit/.bsp.json/.pts/...). Checked
    # before the generic image rules below so a stray map .png goes in this bucket, not another.
    if top == "maps" and MAPS_ALLOWED and map_stem(relslash) not in MAPS_ALLOWED:
        return "map-not-listed"
    if "/" not in relslash and low in SKIP_ROOT_FILES:
        return "root-junk"
    if "/" not in relslash and low.startswith(SKIP_ROOT_PREFIXES):
        return "root-junk"
    # Runtime state living in a subdirectory -- matched on the whole relative path, since the
    # root-only checks above cannot see it.
    if relslash.lower() in SKIP_RELPATHS:
        return "runtime-state"
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
        prune_dirs(dirpath, dirnames, gamedir)
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
    ap.add_argument("--launcher-version", default="0.1.6")
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

    load_maps_allowlist(args.gamedir)

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

    # A stale SKIP_RELPATHS entry does not error -- it just stops matching, and one machine's
    # keybinds and map cache quietly ship to every tester. So check the files it names either
    # got skipped or genuinely are not on disk, and shout if one is present-but-unskipped.
    skipped_rels = {rel.replace(os.sep, "/").lower() for rel, _ in skipped}
    leaked = [
        p for p in sorted(SKIP_RELPATHS)
        if p not in skipped_rels and os.path.exists(os.path.join(args.gamedir, p.replace("/", os.sep)))
    ]
    if leaked:
        print("\n*** RUNTIME STATE IS ABOUT TO BE PUBLISHED ***")
        for p in leaked:
            print(f"      {p}  exists but was not skipped -- SKIP_RELPATHS is stale")
        sys.exit("refusing to publish per-machine state")
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

    # Explicit retirements (things that were never manifest entries -- runtime state that
    # shipped before SKIP_RELPATHS existed) plus everything the previous manifest carried and
    # this one drops.
    new_paths = {e["path"] for e in files}
    cumulative, newly = derive_removed(prev_path, new_paths, platforms)
    removed_paths = sorted(
        (set(REMOVED_PATHS) | load_retired(args.out) | cumulative) - new_paths)
    save_retired(args.out, removed_paths)
    print(f"removed[]    : {len(removed_paths)} paths "
          f"({len(newly)} newly dropped this run, rest carried forward)")

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
        # Consumers that predate this key ignore it (no deny_unknown_fields anywhere in the
        # launcher's manifest structs), so it needs no schema bump.
        "removed": removed_paths,
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

    # Empty files cost a manifest entry, a request and a disk write to deliver nothing, and
    # they are almost always engine scratch that got swept up. Not an error -- some formats
    # do use a zero-length file as a marker -- so this reports rather than drops. Add the
    # name to SKIP_ROOT_FILES if it turns out to be scratch.
    empties = [f["path"] for f in files if f["size"] == 0]
    if empties:
        print(f"\nNOTE: {len(empties)} zero-byte file(s) in this manifest:")
        for p in empties[:10]:
            print(f"        {p}")
        if len(empties) > 10:
            print(f"        ... and {len(empties) - 10} more")

    print(f"done in {dt:.1f}s\n")
    print("next: push with  .\\deploy\\r2-push.ps1   (objects first, manifest last)")
    print("      (see C:\\FTEQuake\\launcher\\deploy\\rclone-and-r2-setup.md)")


if __name__ == "__main__":
    main()
