# Puts the Xtensa linker on PATH for this PowerShell session: dot-source it with
#   . ./v2r/env.ps1
# It finds the newest xtensa-esp-elf GCC that espup (or a manual install) left
# under ~/.rustup/toolchains/esp.
$root = Join-Path $env:USERPROFILE '.rustup\toolchains\esp\xtensa-esp-elf'
$gcc = Get-ChildItem $root -Recurse -Filter 'xtensa-esp32s3-elf-gcc.exe' -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $gcc) {
    Write-Error "No Xtensa GCC under $root. Run: espup install --targets esp32s3"
    return
}
$bin = $gcc.DirectoryName
if (-not ($env:PATH -split ';' -contains $bin)) {
    $env:PATH = "$bin;$env:PATH"
}
Write-Host "Xtensa linker: $($gcc.FullName)"
