$ErrorActionPreference = 'Stop'

Set-Location $PSScriptRoot

if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) {
    throw '未检测到 Rust 编译器。请先完成 Rust 工具链安装或升级。'
}

cargo build --release --package dl --bin dl --locked
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$binary = Join-Path $PSScriptRoot 'target\release\dl.exe'
if (-not (Test-Path -LiteralPath $binary)) {
    throw "构建完成但未找到可执行文件：$binary"
}

Write-Host "构建成功：$binary"
