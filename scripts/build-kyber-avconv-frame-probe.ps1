param()

$ErrorActionPreference = 'Stop'

$container = 'skyber-c1-windows-builder'
$destination = Join-Path $PSScriptRoot '..\target\a2-2\win\skyber-avconv-frame-probe.exe'
$source = Join-Path $PSScriptRoot '..\native\kyber\avconv_frame_source_probe.c'

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw 'Docker CLI was not found. Run scripts/build-kyber-windows.ps1 first.'
}

$state = (& docker inspect $container --format '{{.State.Status}}' 2>$null).Trim()
if ($LASTEXITCODE -ne 0 -or $state -ne 'running') {
    throw "The persistent Kyber builder '$container' is not running. Run scripts/build-kyber-windows.ps1 first."
}

& docker exec $container test -f /tmp/skyber-rootfs/lib/libavconv.a
if ($LASTEXITCODE -ne 0) {
    throw 'The pinned Kyber avconv library is missing from the builder rootfs.'
}

& docker cp $source "${container}:/tmp/avconv_frame_source_probe.c"
if ($LASTEXITCODE -ne 0) { throw 'Could not copy the probe source into the builder.' }

$build = 'export PKG_CONFIG_LIBDIR=/tmp/skyber-rootfs/lib/pkgconfig; x86_64-w64-mingw32-gcc -std=c11 -Wall -Wextra -Werror $(x86_64-w64-mingw32-pkg-config --cflags libavconv) /tmp/avconv_frame_source_probe.c -o /tmp/skyber-avconv-frame-probe.exe $(x86_64-w64-mingw32-pkg-config --libs libavconv)'
& docker exec $container bash -lc $build
if ($LASTEXITCODE -ne 0) { throw 'Kyber avconv probe build failed.' }

$directory = Split-Path -Parent $destination
New-Item -ItemType Directory -Force $directory | Out-Null
& docker cp "${container}:/tmp/skyber-avconv-frame-probe.exe" $destination
if ($LASTEXITCODE -ne 0) { throw 'Could not copy the built probe into target/a2-2/win.' }

Write-Output "Built $destination against the pinned Kyber avconv SDK."
