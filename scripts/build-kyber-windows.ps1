param(
    [switch] $Status
)

$ErrorActionPreference = 'Stop'

$context = 'desktop-linux'
$image = 'registry.gitlab.com/kyber/ops/docker-images/debian-win64@sha256:39632e7b0283d5686cfae181b0c85d17b68e3a3024d4054c5eae64b80d8c9694'
$volume = 'skyber-c1-windows-cache'
$container = 'skyber-c1-windows-builder'
$imageTag = 'skyber-kyber-win64:ebca32a08f6488ea'
$sdkRevision = 'ebca32a08f6488ea23750943ded52119efd584d6'

function Invoke-Docker {
    param([string[]] $DockerArgs)
    & docker @DockerArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Docker command failed with exit code $LASTEXITCODE."
    }
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw 'Docker CLI was not found. Install Docker Desktop and reopen PowerShell.'
}

$env:DOCKER_CONTEXT = $context
& docker info --format '{{.OSType}}/{{.Architecture}}' 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Docker Desktop is not accessible from this PowerShell session. Run this script as the Windows user who started Docker Desktop. If only the agent sandbox is denied, run it from the user PowerShell session.'
}

$engine = (& docker info --format '{{.OSType}}/{{.Architecture}}').Trim()
if ($LASTEXITCODE -ne 0 -or $engine -ne 'linux/x86_64') {
    throw "Expected Docker Linux x86_64 engine, found '$engine'."
}

if ($Status) {
    $containerId = & docker ps --all --quiet --filter "name=^/$container$"
    if ($containerId) {
        & docker inspect $container --format 'container={{.Name}} state={{.State.Status}} image={{.Config.Image}}'
    } else {
        Write-Output 'Persistent builder has not been created.'
    }
    $volumeName = & docker volume ls --quiet --filter "name=^$volume$"
    if ($volumeName) {
        & docker volume inspect $volume --format 'volume={{.Name}}'
    } else {
        Write-Output 'Persistent cache volume has not been created.'
    }
    exit 0
}

foreach ($candidate in @($container, 'skyber-c1-windows-build')) {
    & docker inspect $candidate 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) { continue }
    $candidateState = (& docker inspect $candidate --format '{{.State.Status}}').Trim()
    if ($candidateState -ne 'running') { continue }
    $runningBuild = & docker exec $candidate bash -lc "pgrep -af 'build-win32.sh|meson compile|cargo (build|cinstall)' | grep -v grep" 2>$null
    if ($LASTEXITCODE -eq 0 -and $runningBuild) {
        throw "A Kyber build is already running in '$candidate'. Check it with: docker logs --tail 40 $candidate"
    }
}

$imageExists = & docker image inspect $imageTag 2>$null
if ($LASTEXITCODE -ne 0) {
    $dockerfile = @"
FROM $image
RUN uv tool install --python /usr/bin/python3 --with 'jinja2==3.1.6' --with 'markupsafe==3.0.3' 'meson==1.12.1'
ENV PATH="/root/.local/bin:/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
ENV CARGO_HOME=/tmp/cargo
ENV MESON_PACKAGE_CACHE_DIR=/tmp/meson-packagecache
"@
    $dockerfile | & docker build --tag $imageTag --file - -
    if ($LASTEXITCODE -ne 0) { throw 'Could not build the pinned Kyber toolchain image.' }
}

& docker volume inspect $volume 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) { Invoke-Docker @('volume', 'create', $volume) | Out-Null }

$containerExists = & docker inspect $container 2>$null
if ($LASTEXITCODE -ne 0) {
    Invoke-Docker @('run', '--detach', '--name', $container, '--volume', "${volume}:/tmp", $imageTag, 'sleep', 'infinity') | Out-Null
} else {
    $existingImage = (& docker inspect $container --format '{{.Config.Image}}').Trim()
    if ($existingImage -ne $imageTag) {
        throw "Container '$container' uses '$existingImage'. Review it before recreating it. Cache volume '$volume' is preserved."
    }
    $state = (& docker inspect $container --format '{{.State.Status}}').Trim()
    if ($state -ne 'running') { Invoke-Docker @('start', $container) | Out-Null }
}

$buildScript = @'
set -euo pipefail
export PATH=/root/.local/bin:/cargo/bin:$PATH
export CARGO_HOME=/tmp/cargo
export MESON_PACKAGE_CACHE_DIR=/tmp/meson-packagecache
sdk=/tmp/skyber-kysdk
rootfs=/tmp/skyber-rootfs

mkdir -p "$MESON_PACKAGE_CACHE_DIR" "$CARGO_HOME"
if [ ! -d "$sdk/.git" ]; then
  git clone --recurse-submodules https://gitlab.com/kyber/core/kysdk.git "$sdk"
fi
git -C "$sdk" fetch --tags --recurse-submodules
git -C "$sdk" checkout --detach ebca32a08f6488ea23750943ded52119efd584d6
git -C "$sdk" submodule update --init --recursive
test "$(git -C "$sdk" rev-parse HEAD)" = ebca32a08f6488ea23750943ded52119efd584d6

cd "$sdk/kyctl"
./build-win32.sh -d -o "$rootfs"
./build-win32.sh -s -o "$rootfs" build-kyclient
test -f "$rootfs/lib/libkyclient.a"
find "$rootfs" -maxdepth 3 -type f -print | sort > /tmp/skyber-kyber-windows-artifacts.txt
'@

$buildScript | & docker exec --interactive $container bash -s
if ($LASTEXITCODE -ne 0) {
    throw "Kyber build failed. Sources, Meson downloads, Cargo downloads and logs remain in volume '$volume'. Rerun this script to resume."
}

Write-Output "Kyber dependencies and client built successfully in '$container'. Artifacts are in its persistent cache volume '$volume'."
