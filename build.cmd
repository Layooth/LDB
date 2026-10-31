@echo off
rem ---------------------------------------------------------------------------
rem LittleDB (Rust) 编译包装脚本
rem
rem 为什么需要它：
rem   本机是 Windows Server 2016，而 VS 2026 的 link.exe 依赖 Server 2016
rem   没有的系统 API，直接运行会返回 0xc0000139（STATUS_ENTRYPOINT_NOT_FOUND）。
rem   这里改用 Rust 自带的 rust-lld 链接，绕过 MSVC 链接器。
rem
rem 用法:
rem   build.cmd test
rem   build.cmd run --demo ./rust-demo
rem   build.cmd run -- sql ./data "SHOW TABLES"
rem
rem 如果你的机器上 cargo build 能直接成功（有正常的 VS Build Tools），
rem 可以忽略这个脚本，直接使用 cargo。
rem ---------------------------------------------------------------------------

setlocal

set "CARGO_BIN=%USERPROFILE%\.cargo\bin"
set "PATH=%CARGO_BIN%;%PATH%"

set "RUST_LLD=%USERPROFILE%\.rustup\toolchains\stable-x86_64-pc-windows-msvc\lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe"

if exist "%RUST_LLD%" (
    set "RUSTFLAGS=-Clinker=%RUST_LLD%"
) else (
    echo [build] 未找到 rust-lld，回退到默认链接器
)

cargo %*

endlocal
