param(
    [switch] $Status
)

$ErrorActionPreference = 'Stop'

$context = 'desktop-linux'
$volume = 'skyber-c1-windows-cache'
$container = 'skyber-c1-windows-builder'
$imageTag = 'skyber-kyber-win64:ebca32a08f6488ea'

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
    $candidateId = & docker ps --all --quiet --filter "name=^/$candidate$"
    if (-not $candidateId) { continue }
    $candidateState = (& docker inspect $candidate --format '{{.State.Status}}').Trim()
    if ($candidateState -ne 'running') { continue }
    $runningBuild = & docker exec $candidate bash -lc "pgrep -af 'build-win32.sh|meson compile|cargo (build|cinstall)' | grep -v grep" 2>$null
    if ($LASTEXITCODE -eq 0 -and $runningBuild) {
        throw "A Kyber build is already running in '$candidate'. Check it with: docker logs --tail 40 $candidate"
    }
}

$imageExists = & docker image ls --quiet $imageTag
if (-not $imageExists) {
    $dockerfile = Join-Path $PSScriptRoot 'kyber-windows-toolchain.Dockerfile'
    & docker build --tag $imageTag --file $dockerfile $PSScriptRoot
    if ($LASTEXITCODE -ne 0) { throw 'Could not build the pinned Kyber toolchain image.' }
}

$existingVolume = & docker volume ls --quiet --filter "name=^$volume$"
if (-not $existingVolume) { Invoke-Docker @('volume', 'create', $volume) | Out-Null }

$existingContainer = & docker ps --all --quiet --filter "name=^/$container$"
if (-not $existingContainer) {
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
if ! git -C "$sdk" cat-file -e ebca32a08f6488ea23750943ded52119efd584d6^{commit} 2>/dev/null; then
  git -C "$sdk" fetch --tags --recurse-submodules
fi
if [ "$(git -C "$sdk" rev-parse HEAD)" != ebca32a08f6488ea23750943ded52119efd584d6 ]; then
  git -C "$sdk" checkout --detach ebca32a08f6488ea23750943ded52119efd584d6
fi
git -C "$sdk" submodule update --init --recursive
test "$(git -C "$sdk" rev-parse HEAD)" = ebca32a08f6488ea23750943ded52119efd584d6

cd "$sdk/kyctl"
if [ ! -f "$rootfs/bin/kyavserver.exe" ] || \
   [ ! -f "$rootfs/bin/kynputserver.exe" ] || \
   [ ! -f "$rootfs/lib/libavcodec.a" ]; then
  ./build-win32.sh -d -o "$rootfs"
fi
if [ ! -f "$rootfs/lib/libkyclient.a" ] || [ ! -f "$rootfs/bin/kyclient.dll" ]; then
  ./build-win32.sh -s -o "$rootfs" build-kyclient
fi
test -f "$rootfs/lib/libkyclient.a"
for runtime in libstdc++-6.dll libgcc_s_seh-1.dll libwinpthread-1.dll; do
  runtime_path="$(x86_64-w64-mingw32-gcc -print-file-name="$runtime")"
  test -f "$runtime_path"
  install -m 755 "$runtime_path" "$rootfs/bin/$runtime"
done
find "$rootfs" -maxdepth 3 -type f -print | sort > /tmp/skyber-kyber-windows-artifacts.txt
'@

$buildScript | & docker exec --interactive $container bash -s
if ($LASTEXITCODE -ne 0) {
    throw "Kyber build failed. Sources, Meson downloads, Cargo downloads and logs remain in volume '$volume'. Rerun this script to resume."
}

Write-Output "Kyber dependencies and client built successfully in '$container'. Artifacts are in its persistent cache volume '$volume'."
