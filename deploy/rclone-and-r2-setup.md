# Standing up the mirrors — Cloudflare R2 (primary) + the proto.bar Pi (failover)

One-time setup. After this, every release is `python publish.py --prune` then two `rclone` pushes.

---

## 0. STOP — proto.bar is not on Cloudflare yet

**Every record you added in the Cloudflare dashboard is currently inert.** `proto.bar` still
delegates to Namecheap, so the Cloudflare zone is stuck in *Pending Nameserver Update*: Cloudflare
is not authoritative, nothing is proxied, and **an R2 custom domain cannot be attached to a pending
zone.** Verified live:

```
$ nslookup -type=NS proto.bar 8.8.8.8
proto.bar   nameserver = dns1.registrar-servers.com
proto.bar   nameserver = dns2.registrar-servers.com     <- Namecheap, not Cloudflare

$ curl -sI https://dl.proto.bar/manifests/alpha.json
HTTP/1.1 200 OK
Server: nginx/1.22.1          <- the Pi answering directly; no `cf-ray`, so NOT proxied
```

### Fix it first
1. Cloudflare dashboard → `proto.bar` → **Overview** → copy the two assigned nameservers
   (they look like `xxx.ns.cloudflare.com`).
2. Namecheap → Domain List → `proto.bar` → **Manage** → *Nameservers* → switch
   **Namecheap BasicDNS → Custom DNS** → paste both Cloudflare nameservers → save (the green tick).
3. Wait for the zone to flip to **Active** (usually minutes, up to 24 h). Cloudflare emails you.

**Before you flip, confirm the MX + SPF records are in the Cloudflare zone**, or email forwarding
for `@proto.bar` breaks the moment the nameservers change. You already have all five
`eforward1-5.registrar-servers.com` MX records and the SPF TXT — just verify they are still there
and **DNS only** (grey cloud). Mail can never be proxied.

Nothing below works until the zone says Active.

---

## 1. Decide the hostnames

Three jobs, three names. This is the split the configs in this directory assume:

| Hostname | Points at | Proxy | Why |
|---|---|---|---|
| `dl.proto.bar` | **Cloudflare R2** | Orange (R2 manages it) | Primary. $0 egress, global edge, absorbs the launch spike. |
| `files.proto.bar` | The Pi (`180.150.62.57`) | **Grey (DNS only)** | Failover. See the ToS note in §6. |
| `play.proto.bar` | The Pi (`180.150.62.57`) | **Grey (DNS only)** | The game server. **Must** be grey — see §5. |

You already have `dl.proto.bar` as a **grey A record to the Pi**. To hand that name to R2:

- **Delete the `dl.proto.bar` A record first.** Attaching an R2 custom domain to a hostname that
  already has an A/CNAME fails with API error **10056 — "DNS record for this domain already exists
  on zone."** R2 creates and manages its own record; do not hand-create it.
- Then **add `files.proto.bar` → A → `180.150.62.57`, DNS only**, so the Pi mirror keeps a name.
- Add `play.proto.bar` → A → `180.150.62.57`, **DNS only**, and point players at that.

> Prefer to keep `dl.proto.bar` on the Pi? Then give R2 a different name (e.g. `cdn.proto.bar`) and
> swap the order in `launcher.toml`. Only the mirror URLs change; nothing else cares.

---

## 2. Cloudflare R2 (primary)

1. Dashboard → **R2 object storage**. First use asks you to add a payment method even on the free
   tier — expected; see the cost estimate in §7 (it comes to **$0.00/month** at this scale).
2. **Create bucket** → name it `quakers-dl` → location hint *Automatic* → **Standard** storage class.
3. Attach the domain: **R2 object storage → `quakers-dl` → Settings → Custom Domains → Add** →
   enter `dl.proto.bar` → **Continue** → review the DNS record → **Connect Domain**.
   Status goes *Initializing → Active* in a few minutes. TLS is issued automatically.
4. Do **not** use the `r2.dev` development URL for real traffic — it is rate-limited, explicitly
   "intended for non-production traffic", and cannot be cached.
5. **Turn caching on.** For a custom domain, only certain file types are cached by default, and our
   blobs are extensionless hash names. Add a Cache Rule so the immutable objects actually sit at the
   edge instead of hitting R2 on every request:
   - **Rules → Cache Rules → Create rule**
   - Name: `quakers objects immutable`
   - When: *Hostname equals* `dl.proto.bar` *and* *URI Path starts with* `/objects/`
   - Then: **Eligible for cache**, Edge TTL **1 year**, Browser TTL **1 year**
   - Second rule for `/manifests/`: **Eligible for cache**, Edge TTL **30 seconds** — or just leave
     manifests uncached; they are ~1 MB and fetched once per launch.

### R2 API token (for rclone)
**R2 → API → Manage API Tokens → Create API Token** → permission **Object Read & Write** → scope it
to the `quakers-dl` bucket. Save the **Access Key ID** and **Secret Access Key** (shown once). Your
**Account ID** is on the R2 overview page.

---

## 3. rclone remotes

Install rclone (<https://rclone.org/downloads/>) — it is **not** currently installed on this machine.
On Windows: `winget install Rclone.Rclone`. Then `rclone config`:

**Remote `r2`** — type `s3`, provider `Cloudflare`:
```
type = s3
provider = Cloudflare
access_key_id     = <R2 access key>
secret_access_key = <R2 secret>
endpoint = https://<ACCOUNT_ID>.r2.cloudflarestorage.com
region = auto
acl = private
```

**Remote `pi`** — type `sftp`: host `files.proto.bar`, user `<pi user>`, key_file `~/.ssh/id_ed25519`.

> Windows/MSYS trap: prefix path-bearing commands with `MSYS_NO_PATHCONV=1` so `/srv/...` is not
> rewritten into a Windows path. The same trap applies to `wsl` invocations — see `build-linux.sh`.

---

## 4. The Pi failover mirror

The Pi is **already serving** the tree over HTTPS with a valid Let's Encrypt cert and Range support
(verified: `206 Partial Content` on a byte-range request), so this half is essentially done — it
just needs to move to `files.proto.bar`:

```
sudo cp nginx-quakers-dl.conf /etc/nginx/sites-available/quakers-dl.conf
sudo ln -s /etc/nginx/sites-available/quakers-dl.conf /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d files.proto.bar
```

Content lives at `/srv/nvme/quakers/dl/{objects,manifests}` (the NVMe — the 7 GB root overlay
cannot hold a 6.3 GB payload, which is also why MinIO was dropped).

**Keep downloads off the game server.** The Pi's 40 Mbps uplink also carries live play. R2 is primary
so this box should be near-idle, but cap it anyway: `limit_rate 2m;` is already in the vhost (per
*connection*, and the launcher opens 8), plus router QoS prioritising the game's UDP port over TCP 443.

---

## 5. ⚠ The game server must NOT be proxied

Cloudflare's proxy carries **TCP only**, on a fixed port list — HTTP `80, 8080, 8880, 2052, 2082,
2086, 2095` and HTTPS `443, 2053, 2083, 2087, 2096, 8443`. **It passes no UDP at all** on any
self-serve plan; UDP requires Spectrum, which is Enterprise-only.

FTE's server is UDP — `PORT_QWSERVER 27500` (`engine/common/protocol.h:171`). So if players connect
to an **orange-clouded** hostname, their client resolves it to a Cloudflare anycast IP and fires UDP
27500 into a black hole. **The server becomes unreachable.**

You currently have `proto.bar` itself set to Proxied. Once the nameservers flip, that will take the
game server offline for anyone connecting to `proto.bar`. Either grey-cloud `proto.bar`, or (better)
publish `play.proto.bar` as a **DNS-only** record and point players there.

---

## 6. Terms of service: proxy R2, not the Pi

Cloudflare's old "Section 2.8" is gone; the restriction now lives in the
[Service-Specific Terms](https://www.cloudflare.com/service-specific-terms-application-services/):
Cloudflare *"reserves the right to disable or limit your access to or use of the CDN … if you use or
are suspected of using the CDN … to serve video or a disproportionate percentage of pictures, audio
files, or other large files."*

It is discretionary enforcement, not a hard block — but 6.3 GB of `.pk3`/`.dds` per tester is exactly
the pattern it describes. Cloudflare's [own blog](https://blog.cloudflare.com/updated-tos/) draws the
line clearly: *"Customers can serve video and other large files using the CDN so long as that content
is hosted by a Cloudflare service like Stream, Images, or R2."*

**So: R2 through the orange cloud is the sanctioned path. The Pi mirror should stay grey-clouded.**

---

## 7. Cost, limits, and the two oversized pk3s

**Cost: $0.00/month.** R2 free tier is 10 GB-month storage, 1 M Class A ops, 10 M Class B ops. This
payload is 6.3 GB / 5,101 files. A full upload is ~5,100 Class A ops; 100 full installs is ~510,000
Class B ops. **Egress is $0 at any volume** — that is the entire reason for R2, and it applies to
custom domains too (Cloudflare's CDN bandwidth is unmetered on every plan).
Rates past the free tier: $0.015/GB-month, $4.50/M Class A, $0.36/M Class B.

**512 MB edge-cache ceiling (Free/Pro/Business).** Two objects exceed it:

| File | Size | Effect |
|---|---|---|
| `quakers/polyhaven_props.pk3` | 2.12 GB | Served normally, **never edge-cached** |
| `quakers/polyhaven.pk3` | 781 MB | Served normally, **never edge-cached** |

They still download fine and still cost $0 in egress — they just come from R2 on every request
instead of the edge, so they are slower and burn a Class B op each time. Nothing to fix for the
alpha. If it ever matters, split them into <512 MB pk3s at pack time.

**Timeouts.** The proxy's default **Proxy Read Timeout is 125 s** (the widely repeated "100 s" is
stale) and it governs how long the *origin* may take to start responding — it does not kill an
in-flight streaming download, so multi-GB objects are fine. The launcher resumes with HTTP Range
regardless, so a dropped transfer costs only the un-received tail.

**The 100 MB upload limit does not apply** — that caps request *bodies* through the proxy. rclone
uploads to the S3 endpoint, not through the CDN.

---

## 8. Publish a release

From `C:\FTEQuake\launcher`:

```bash
# 0. Linux artifacts, if the engine or launcher changed:
MSYS_NO_PATHCONV=1 wsl -d Ubuntu-22.04 -- bash /mnt/c/FTEQuake/launcher/deploy/build-linux.sh

# 1. Build manifest + object tree (both platforms):
python publish.py --prune --channel alpha --version 2026.07.27_1

# 2. Objects FIRST (content-addressed, so this is purely additive):
rclone sync dist/objects   r2:quakers-dl/objects   --checksum --transfers=16 --fast-list
rclone sync dist/objects   pi:/srv/nvme/quakers/dl/objects --checksum --transfers=8

# 3. The launcher binaries themselves (only when the launcher changes — see below):
rclone copy dist/launcher  r2:quakers-dl/launcher
rclone copy dist/launcher  pi:/srv/nvme/quakers/dl/launcher

# 4. Manifest LAST — this is the atomic switch that makes the release live:
rclone copy dist/manifests r2:quakers-dl/manifests
rclone copy dist/manifests pi:/srv/nvme/quakers/dl/manifests
```

### ⚠ Purge the CDN after step 3 — `.exe` is cached for 4 hours

Content blobs are safe: their names are hashes, so a changed file is a *different* URL and can
never be served stale. The **launcher binaries are not** — `quakers-launcher.exe` keeps the same
URL forever, and `.exe` is on Cloudflare's default-cached extension list. Observed on the
2026-07-27 alpha push, minutes after uploading a new build:

```
Content-Length: 3695616      ← the PREVIOUS build
Age: 1032
Cache-Control: max-age=14400 ← 4 hours
cf-cache-status: HIT
```

The origin had the new 3732992-byte binary the whole time; every tester would have downloaded the
old one for the next four hours. `quakers-launcher` (Linux, no extension) is *not* cached and
updated immediately — so the two platforms silently drift apart.

After pushing `dist/launcher/`, purge those URLs:

- Dashboard → **Caching → Configuration → Purge Cached Content → Custom Purge**, then paste:
  `https://dl.proto.bar/launcher/quakers-launcher.exe`
  `https://dl.proto.bar/launcher/quakers-launcher`
- Verify with a cache-buster, which always bypasses the edge:
  ```bash
  curl -sI "https://dl.proto.bar/launcher/quakers-launcher.exe?cb=$RANDOM" | grep -i content-length  # origin
  curl -sI "https://dl.proto.bar/launcher/quakers-launcher.exe"            | grep -iE 'content-length|cf-cache-status'
  ```
  The two `content-length` values must match. If they don't, the purge hasn't taken.

The permanent fix is to give the launcher a versioned filename
(`quakers-launcher-2026.07.27.exe`) plus a stable redirect, so it becomes immutable like the
blobs. Until then, purging is a required step of every launcher release.

### How testers get the launcher

The launcher is the *bootstrap*, so it is deliberately **not** in the manifest — it cannot download
itself before it exists. `dist/launcher/` holds what testers need; after the push above the links are:

| Platform | Link to hand out |
|---|---|
| Windows | `https://dl.proto.bar/launcher/quakers-launcher.exe` |
| Linux | `https://dl.proto.bar/launcher/quakers-launcher` |
| both | `https://dl.proto.bar/launcher/launcher.toml` (optional — mirrors are baked in) |

Tell them: put it in an empty folder and run it. On Linux, `chmod +x quakers-launcher` first — that
is the *only* chmod a tester ever has to do; the launcher sets `+x` on everything it downloads.

Windows SmartScreen will warn on an unsigned exe ("Windows protected your PC" → *More info* → *Run
anyway*). Code-signing removes that and is on the M3 list.

`--checksum` means only blobs whose hash-named object is missing get uploaded, so a patch release
pushes just what changed (a new `csprogs.dat` is ~8 MB, not 6.3 GB). Publishing the manifest last
guarantees a client never sees a manifest referencing an object that is not uploaded yet.

**Roll back** by re-copying a previous manifest — keep dated copies, e.g.
`manifests/alpha-2026.07.27_1.json`.

---

## 9. Validate without the launcher

```bash
curl -sI https://dl.proto.bar/manifests/alpha.json          # 200
curl -s  https://dl.proto.bar/manifests/alpha.json | python -c "import sys,json;m=json.load(sys.stdin);print(m['schema'],m['total_files'],'files',round(m['total_bytes']/1e9,2),'GB',m.get('platforms'))"

# an object, and prove Range/resume works (must be 206):
H=<some hash from the manifest>
curl -sI -r 0-99 "https://dl.proto.bar/objects/${H:0:2}/$H"

# confirm it is really coming through Cloudflare and caching:
curl -sI "https://dl.proto.bar/objects/${H:0:2}/$H" | grep -iE 'cf-ray|cf-cache-status'
```
Then the real test, on each platform:
```
quakers-launcher --dry-run          # should list ONLY your platform's engine files
```
