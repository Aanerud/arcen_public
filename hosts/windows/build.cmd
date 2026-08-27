@echo off
setlocal EnableExtensions

set "ROOT=%~dp0..\.."
for %%I in ("%ROOT%") do set "ROOT=%%~fI"
rem The Rust toolchain VERSION comes from rust-toolchain.toml so there is one
rem source of truth; the ABI suffix is forced here because rustup resolves the
rem host triple separately, and this machine's default host is windows-gnu.
rem arcen-credential-provider is a COM DLL loaded by LogonUI and must be MSVC.
rem
rem Parsed with plain batch rather than PowerShell: the version is read before
rem any interpreter is known to be usable, and nesting quotes through
rem powershell -Command is a reliable source of parser errors.
set "TOOLCHAIN_LINE="
for /f "usebackq tokens=1,* delims==" %%A in (`findstr /b /r /c:"channel[ ]*=" "%ROOT%\rust-toolchain.toml"`) do set "TOOLCHAIN_LINE=%%B"
if not defined TOOLCHAIN_LINE (
  echo error: no channel entry found in "%ROOT%\rust-toolchain.toml".
  exit /b 1
)
rem `TOOLCHAIN_LINE` is now ` "1.96.1"`: drop spaces, then drop the quotes. The
rem quote-stripping `set` is deliberately unquoted, because the search pattern
rem itself is a quote character.
set "TOOLCHAIN_LINE=%TOOLCHAIN_LINE: =%"
set TOOLCHAIN_VERSION=%TOOLCHAIN_LINE:"=%
if not defined TOOLCHAIN_VERSION (
  echo error: could not parse the channel value from rust-toolchain.toml.
  exit /b 1
)
set "TOOLCHAIN=%TOOLCHAIN_VERSION%-x86_64-pc-windows-msvc"
set "CARGO=%USERPROFILE%\.cargo\bin\cargo.exe"
if not defined RUSTC set "RUSTC=%USERPROFILE%\.cargo\bin\rustc.exe"
set "DIST=%ROOT%\target\arcen-windows-x64"
set "PACKAGE_TARGET=%ROOT%\target\windows-package"

if not exist "%CARGO%" set "CARGO=cargo"
if not exist "%RUSTC%" set "RUSTC=rustc"

rem Enter the MSVC build environment ourselves rather than demanding the caller
rem already be in one. packaging\windows\with-msvc.cmd was written to locate
rem Visual Studio and hand its environment to a command, but nothing ever called
rem it, so this script still failed with "link.exe is required" for any caller
rem that was not an interactive Developer Command Prompt -- an SSH session, a
rem scheduled build, a CI step. Re-invoke once through it, guarded so the second
rem pass cannot recurse.
where link.exe >nul 2>nul
if errorlevel 1 (
  if not defined ARCEN_MSVC_BOOTSTRAPPED (
    set "ARCEN_MSVC_BOOTSTRAPPED=1"
    call "%ROOT%\packaging\windows\with-msvc.cmd" "%~f0" %*
    exit /b %ERRORLEVEL%
  )
)

for %%T in (link.exe cl.exe lib.exe dumpbin.exe cmake.exe) do (
  where %%T >nul 2>nul
  if errorlevel 1 (
    echo error: %%T is required; run from an x64 Visual Studio Developer Command Prompt with CMake on PATH.
    exit /b 1
  )
)
rem Announced after the bootstrap so it is printed once, by the pass that
rem actually builds, rather than once per re-invocation.
echo Building with Rust toolchain %TOOLCHAIN%
cmake --version
if errorlevel 1 exit /b %errorlevel%
set "RUST_HOST="
set "RUST_HOST_FILE=%TEMP%\arcen-rust-host-%RANDOM%-%RANDOM%.txt"
call "%RUSTC%" +%TOOLCHAIN% --print host-tuple >"%RUST_HOST_FILE%"
if errorlevel 1 (
  del /q "%RUST_HOST_FILE%" >nul 2>nul
  exit /b 1
)
set /p "RUST_HOST="<"%RUST_HOST_FILE%"
del /q "%RUST_HOST_FILE%" >nul 2>nul
if not "%RUST_HOST%"=="x86_64-pc-windows-msvc" (
  echo error: %TOOLCHAIN% did not report the required x86_64-pc-windows-msvc host.
  exit /b 1
)
cd /d "%ROOT%"
if errorlevel 1 exit /b %errorlevel%

set "RUSTFLAGS=-C target-feature=+crt-static"
set "CFLAGS_x86_64_pc_windows_msvc=/MT"
set "CXXFLAGS_x86_64_pc_windows_msvc=/MT"
if exist "%PACKAGE_TARGET%" rmdir /s /q "%PACKAGE_TARGET%"
set "CARGO_TARGET_DIR=%PACKAGE_TARGET%"

powershell -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\scripts\verify-opusic-source.ps1" -RepositoryRoot "%ROOT%"
if errorlevel 1 exit /b %errorlevel%
powershell -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\hosts\windows\driver\arcen-microphone\verify-driver-source.ps1"
if errorlevel 1 exit /b %errorlevel%
call "%ROOT%\hosts\windows\driver\arcen-microphone\test-driver-portable.cmd"
if errorlevel 1 exit /b %errorlevel%

"%CARGO%" +%TOOLCHAIN% build --locked --release -p arcen-pier-windows -p arcen-credential-provider
if errorlevel 1 exit /b %errorlevel%
python "%ROOT%\scripts\verify_quic_product_binary.py" "%PACKAGE_TARGET%\release\arcen-pier.exe"
if errorlevel 1 exit /b %errorlevel%
set "ARCEN_INSTALLER_PIER_EXE=%PACKAGE_TARGET%\release\arcen-pier.exe"
set "ARCEN_INSTALLER_CP_DLL=%PACKAGE_TARGET%\release\arcen_credential_provider.dll"
"%CARGO%" +%TOOLCHAIN% build --locked --release --manifest-path "%ROOT%\packaging\windows\installer\Cargo.toml"
if errorlevel 1 exit /b %errorlevel%
python "%ROOT%\scripts\verify_quic_product_binary.py" "%PACKAGE_TARGET%\release\install-arcen-pier.exe"
if errorlevel 1 exit /b %errorlevel%

set "ARCEN_WDK_AVAILABLE="
set "ARCEN_WDK_MSBUILD="
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if exist "%VSWHERE%" (
  for /f "usebackq tokens=*" %%I in (`"%VSWHERE%" -latest -products * -requires Component.Microsoft.Windows.DriverKit -find MSBuild\**\Bin\amd64\MSBuild.exe`) do (
    if not defined ARCEN_WDK_MSBUILD set "ARCEN_WDK_MSBUILD=%%I"
  )
)
if exist "%ProgramFiles(x86)%\Windows Kits\10\Include\10.0.26100.0\km\portcls.h" if defined ARCEN_WDK_MSBUILD set "ARCEN_WDK_AVAILABLE=1"
if defined ARCEN_WDK_AVAILABLE (
  "%ARCEN_WDK_MSBUILD%" "%ROOT%\hosts\windows\driver\arcen-microphone\arcen-microphone.vcxproj" /m /t:Build /p:Configuration=Release /p:Platform=x64 /nologo /verbosity:minimal
  if errorlevel 1 exit /b %errorlevel%
) else (
  echo WDK 10.0.26100 with a Visual Studio DriverKit component is unavailable: PortCls source passed static validation; native driver build not run.
)

if exist "%DIST%" rmdir /s /q "%DIST%"
mkdir "%DIST%"
if errorlevel 1 exit /b %errorlevel%

copy /y "%PACKAGE_TARGET%\release\arcen-pier.exe" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%PACKAGE_TARGET%\release\arcen_credential_provider.dll" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%PACKAGE_TARGET%\release\arcen-cp-harness.exe" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%PACKAGE_TARGET%\release\install-arcen-pier.exe" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%ROOT%\hosts\windows\credential-provider\install.ps1" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%ROOT%\hosts\windows\credential-provider\install-test.ps1" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%ROOT%\hosts\windows\credential-provider\registration-common.ps1" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%ROOT%\hosts\windows\credential-provider\uninstall.ps1" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%ROOT%\hosts\windows\INSTALL.md" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%
copy /y "%ROOT%\legal\THIRD_PARTY_NOTICES.md" "%DIST%\" >nul
if errorlevel 1 exit /b %errorlevel%

if defined ARCEN_SIGNED_MICROPHONE_PACKAGE (
  powershell -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\packaging\windows\stage-signed-microphone-driver.ps1" -SourceDirectory "%ARCEN_SIGNED_MICROPHONE_PACKAGE%" -DistributionDirectory "%DIST%" -RepositoryRoot "%ROOT%"
  if errorlevel 1 exit /b %errorlevel%
  powershell -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\packaging\windows\verify-package-manifest.ps1" -DistributionDirectory "%DIST%"
  if errorlevel 1 exit /b %errorlevel%
) else (
  echo Protected signed microphone package was not supplied; target output is not a release package.
)

if defined ARCEN_SIGNED_MICROPHONE_PACKAGE (
  powershell -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\packaging\windows\verify-static-runtime.ps1" -RepositoryRoot "%ROOT%" -DistributionDirectory "%DIST%" -CargoTargetDirectory "%PACKAGE_TARGET%"
) else (
  powershell -NoProfile -ExecutionPolicy Bypass -File "%ROOT%\packaging\windows\verify-static-runtime.ps1" -RepositoryRoot "%ROOT%" -DistributionDirectory "%DIST%" -CargoTargetDirectory "%PACKAGE_TARGET%" -DriverlessBuild
)
if errorlevel 1 exit /b %errorlevel%

echo Built self-contained Windows x64 artifacts:
echo   %DIST%
endlocal
