# Standing up the mirrors — Cloudflare R2 (primary) + the proto.bar Pi (failover)

One-time setup. After this, every release is `python publish.py --prune` then two `rclone` pushes.

---

## 0. Status — the zone is Active; R2 is the remaining piece

The nameserver cutover completed **2026-07-27**. `dl.proto.bar` is proxied and Cloudflare is
authoritative, so an R2 custom domain *can* now be attached.

### Why R2 stopped being optional

On the evening of 2026-07-27 a single tester's ~6 GB install took the house connection down.
The instinct was that the launcher had bypassed the CDN. It had not — every origin request in
`/var/log/nginx/access.log` came from a Cloudflare edge IP (`172.68.x`), UA
`quakers-launcher/0.1.0`. Measured over that one session:

```
requests to origin        9,801
unique objects among them 3,316
re-fetch ratio            2.96x        <- Cloudflare pulled each object ~3 times
bytes off the Pi          6.20 GB      <- for a 5.84 GB payload
peak                      ~14.5 MB/s
```

**Cloudflare's cache is per-edge-server, not per-account.** The launcher opens 8 parallel
workers; those land on different machines within the same PoP, each with its own cache, each
missing independently, each fetching from origin. Caching in front of a home connection does not
remove the home connection from the path — it only reduces how often it is used, and with
parallel cold fetches it can *amplify* instead. Two mitigations, in order of effect:

1. **R2** (this document). The origin is Cloudflare's own storage, so there is no home uplink to
   saturate at all. This is the fix.
2. **Tiered Cache** (Caching → Tiered Cache → Smart Tiered Cache, free). Funnels edge misses
   through one upper-tier PoP so origin sees each object once rather than once per edge server.
   Worth enabling regardless — it also cuts R2 Class B ops.

Note also `limit_rate` in the Pi vhost was commented out and labelled *"Optional"*. It was not
optional; nothing capped egress. It is now uncommented with `limit_conn` alongside it.

---

## 1. Decide the hostnames

Three jobs, three names. This is the split the configs in this directory assume:

| Hostname | Points at | Proxy | Why |
|---|---|---|---|
| `dl.proto.bar` | **Cloudflare R2** | Orange (R2 manages it) | Everything the launcher fetches. $0 egress, no home uplink in the path. |
| `play.proto.bar` | The Pi (`180.150.62.57`) | **Grey (DNS only)** | The game server. **Must** be grey — see §5. |

**Keep `dl.proto.bar` as the name and give it to R2.** That is deliberate: `launcher.toml` in the
already-shipped v0.1.1 build has `https://dl.proto.bar` baked in, so moving the *hostname* rather
than changing the *URL* means every launcher already in someone's hands switches to R2 with no
re-release and no action from the tester. Putting R2 on a new name (`cdn.proto.bar`) would strand
every copy already downloaded.

Serve the launcher binaries from R2 too (`/launcher/*`), not just `/objects/` — otherwise the Pi
stays in the path for the one file every tester fetches first.

There is no Pi mirror in this table on purpose. A second mirror is worth adding when a second
*host* exists; a fallback that points back at the home connection re-creates the exact failure
this migration is fixing, just less often.

`dl.proto.bar` is currently a **proxied A record to the Pi**. To hand that name to R2:

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

**512 MB edge-cache ceiling (Free/Pro/Business).** **No object exceeds it any more.** The two
that did — `polyhaven_props.pk3` (2.12 GB) and `polyhaven.pk3` (781 MB) — were split with
`quakers/tools/split_pk3.py`; the largest object is now 451.8 MB. Verified against the live
`alpha` manifest: 0 entries over 536,870,912 bytes.

Keep it that way. An oversized pack is not an error and produces no warning — it simply never
caches, so it comes off origin on **every single request** while everything around it caches
normally. `split_pk3.py` bin-packs on compressed size under a 450 MiB cap and warns if any part
lands over 512 MB; run it whenever a pack grows.

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

# 2. Push everything to R2 -- objects, then launcher, then manifest last, which is the
#    atomic switch that makes the release live. Ordering is enforced by the script:
.\deploy\r2-push.ps1

#    Useful variants:
.\deploy\r2-push.ps1 -DryRun          # show what would transfer
.\deploy\r2-push.ps1 -BwLimit 0       # no upload throttle (only when nobody is playing)
```

The script uses `rclone copy`, never `sync`. `sync` deletes on the destination, so a half-built
`dist/` — or one bad `--exclude` — would remove live objects out from under testers who are
mid-download. Orphans get pruned deliberately and separately, well after a manifest has gone live.

**Measured on the first full push (2026-07-27):** 4,107 objects / 5.84 GB in **51 minutes** at
~1.95 MiB/s, throttled with `-BwLimit 2M`. The throttle was not the binding constraint — the home
upstream was. Verified afterwards with `rclone check --size-only` (zero differences) plus a
BLAKE2b-256 round-trip on three objects including a 474 MB multipart upload.

> **`--bind 0.0.0.0` is not optional on this network.** AAAA records resolve but there is no
> working IPv6 path, and Go's happy-eyeballs will try v6 first. The symptom is
> `tls: handshake failure` from rclone, which looks exactly like a bad API token and is not.
> Confirmed by `curl -6` failing to `dl.proto.bar` as well. It is baked into `r2-push.ps1`.

> **`no_check_bucket = true` is required for a bucket-scoped token.** rclone otherwise probes with
> `CreateBucket` before its first upload and gets a 403 — again indistinguishable from bad
> credentials. Set on the `r2` remote already.

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
- Verify by **hashing the bytes**, not by comparing `content-length`:
  ```bash
  curl -s "https://dl.proto.bar/launcher/quakers-launcher.exe" | sha256sum
  sha256sum dist/launcher/quakers-launcher.exe
  ```
  They must match. `content-length` is not a sufficient check and gave a false pass on the
  0.1.0 → 0.1.1 push: both builds were **exactly** 3732992 bytes, because the only source
  change was a version string of the same length. A stale HIT looked identical to a fresh one
  on every header — `age` was the sole tell, and `age` alone cannot distinguish "stale" from
  "purged, then re-cached seconds ago".

### Setting Cache-Control on the object is NOT enough

`r2-push.ps1` uploads the launcher binaries with `Cache-Control: public, max-age=300`, and R2
stores it — confirmed with `rclone lsjson -M`. Cloudflare then **overrides it on the way out**:

```
R2 object metadata : cache-control = "public, max-age=300"
what clients get   : Cache-Control: public, max-age=14400     <- 4 hours
```

14400 s is Cloudflare's default **Browser Cache TTL**, which replaces the origin's header
unless the zone is set to *Respect Existing Headers*. So the short TTL that was supposed to make
launcher pushes self-correct does nothing.

**Fix it once, in the dashboard:** Caching → Configuration → **Browser Cache TTL** →
**Respect Existing Headers**. This is safe for everything else here: `/objects/` blobs are
content-addressed and ship `max-age=31536000, immutable`, and manifests ship `max-age=60`.
Both are more correct than a blanket 4 hours.

Optionally add a Cache Rule for `/launcher/` with a short Edge TTL, so the *edge* copy also
expires quickly rather than only the browser copy.

Until Browser Cache TTL is changed, **purging after every launcher push is mandatory** — and note
the stale copy can outlive the origin it came from: after the R2 migration the edge kept serving
the Pi-era binary (identifiable by its nginx `etag` and `last-modified`) for hours.

The other permanent fix is a versioned filename plus a stable redirect, making the launcher
immutable like the blobs. The GitHub release assets already are — see §8.

### How testers get the launcher

The launcher is the *bootstrap*, so it is deliberately **not** in the manifest — it cannot download
itself before it exists. `dist/launcher/` holds what testers need; after the push above the links are:

| Platform | Link to hand out |
|---|---|
| Windows | `https://dl.proto.bar/launcher/quakers-launcher.exe` |
| Linux | `https://dl.proto.bar/launcher/quakers-launcher` |
| both | `https://dl.proto.bar/launcher/launcher.toml` (optional — mirrors are baked in) |

**Or hand out the GitHub release instead** — <https://github.com/ProtoAus/Quakers-Launcher/releases>.
Assets there are versioned per tag (`quakers-launcher-v0.1.1-windows-x64.exe`), so they are
immutable and cannot go stale: it is the versioned-filename fix above, already available, without
needing the redirect. The `dl.proto.bar` links stay because they are stable and short, but they are
the ones that require the purge.

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
