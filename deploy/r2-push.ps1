# =============================================================================
# Push the published tree to Cloudflare R2.
#
#   .\deploy\r2-push.ps1                 # normal run
#   .\deploy\r2-push.ps1 -DryRun         # show what would transfer, move nothing
#   .\deploy\r2-push.ps1 -BwLimit 0      # no throttle (only when nobody is playing)
#
# Prereq: an rclone remote named `r2`. Create it WITHOUT putting the secret in your
# shell history or in a chat transcript -- `rclone config` prompts for it:
#
#   rclone config
#     n) New remote            name> r2
#     Storage> s3              provider> Cloudflare
#     access_key_id> <paste>   secret_access_key> <paste>
#     endpoint> https://<ACCOUNT_ID>.r2.cloudflarestorage.com
#     region> auto             acl> private
#
# WHY THIS EXISTS: on 2026-07-27 a single tester's 6.3 GB download pulled 6.2 GB
# through the Pi's uplink and took the house offline. Cloudflare was in the path the
# whole time -- it just re-fetched each object ~3x from origin, because its cache is
# per-edge-server and the launcher runs 8 parallel workers. R2 removes the origin from
# the picture entirely: egress is free and there is no home connection behind it.
# =============================================================================
param(
    [string] $Bucket  = 'quakers-dl',
    [string] $Remote  = 'r2',
    [string] $Dist    = 'C:\FTEQuake\launcher\dist',
    # Throttle the UPLOAD. This runs over the same home connection the game server
    # uses, so an unthrottled 5.8 GB push is the same outage in the other direction.
    # 3M ~= 24 Mbps. Set 0 to disable.
    [string] $BwLimit = '3M',
    [switch] $DryRun
)

$ErrorActionPreference = 'Stop'
if (-not (Get-Command rclone -ErrorAction SilentlyContinue)) { throw "rclone not on PATH" }
if (-not ((rclone listremotes) -match "^${Remote}:")) {
    throw "rclone remote '$Remote' does not exist. Run 'rclone config' first (see header)."
}

$common = @(
    '--transfers', '8',
    '--checkers', '16',
    '--s3-chunk-size', '32M',
    # R2 has no per-object size ceiling to design around, but multipart still needs a
    # sane part count for the ~450 MB pk3s.
    '--s3-upload-concurrency', '4',
    '--stats', '20s',
    '--stats-one-line',
    '--progress'
)
if ($BwLimit -ne '0') { $common += @('--bwlimit', $BwLimit) }
if ($DryRun)          { $common += '--dry-run' }

function Push-Tree {
    param($Sub, $ExtraArgs)
    $src = Join-Path $Dist $Sub
    if (-not (Test-Path $src)) { Write-Host "skip $Sub (not present)"; return }
    Write-Host "`n=== $Sub -> ${Remote}:$Bucket/$Sub ===" -ForegroundColor Cyan
    # `copy`, not `sync`: sync would delete on the destination, and a bad --exclude or a
    # half-built dist/ would then wipe live objects that testers are mid-download on.
    # Orphans are pruned deliberately, separately, after a manifest has been live a while.
    & rclone copy $src "${Remote}:$Bucket/$Sub" @common @ExtraArgs
    if ($LASTEXITCODE -ne 0) { throw "rclone copy failed for $Sub (exit $LASTEXITCODE)" }
}

# Objects are content-addressed, therefore immutable: a changed file is a different key
# and can never be served stale. Year-long immutable caching is safe and is what keeps
# repeat downloads off R2's Class B op count.
Push-Tree 'objects' @('--header-upload', 'Cache-Control: public, max-age=31536000, immutable')

# Manifests must NOT be cached hard -- this is how the launcher discovers a new build.
Push-Tree 'manifests' @('--header-upload', 'Cache-Control: public, max-age=60')

# The launcher binaries keep a stable URL forever, so they are the one thing that can go
# stale. Short TTL instead of the 4-hour default that bit us on the 0.1.0 -> 0.1.1 push.
Push-Tree 'launcher' @('--header-upload', 'Cache-Control: public, max-age=300')

Write-Host "`n=== remote totals ===" -ForegroundColor Cyan
& rclone size "${Remote}:$Bucket"
