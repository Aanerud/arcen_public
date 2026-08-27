@echo off
setlocal EnableExtensions
rem with-msvc: run a command inside the MSVC x64 build environment.
rem
rem hosts\windows\build.cmd and the microphone driver ring test both start with
rem `where cl.exe` and refuse to run without it, because they are written for a
rem Visual Studio Developer Command Prompt. A GitHub Actions `cmd` step is not
rem one, so those steps failed with "error: cl.exe is required" having done no
rem work at all.
rem
rem Rather than teach each script to find Visual Studio, locate it once here and
rem hand the environment to whatever command is passed. Developers get the same
rem behaviour locally from an ordinary shell.
rem
rem Usage:  packaging\windows\with-msvc.cmd <command> [args...]

if "%~1"=="" (
  echo error: with-msvc requires a command to run. 1>&2
  exit /b 2
)

rem Already inside a Developer Command Prompt: use it rather than nesting.
where cl.exe >nul 2>nul
if not errorlevel 1 goto :run

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
  echo error: vswhere.exe not found at "%VSWHERE%"; cannot locate Visual Studio. 1>&2
  exit /b 1
)

set "VSINSTALL="
for /f "usebackq tokens=*" %%I in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSINSTALL=%%I"
if not defined VSINSTALL (
  echo error: no Visual Studio install with the x64 C++ toolset was found. 1>&2
  exit /b 1
)

set "VCVARS=%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VCVARS%" (
  echo error: vcvars64.bat not found at "%VCVARS%". 1>&2
  exit /b 1
)

call "%VCVARS%" >nul
if errorlevel 1 (
  echo error: vcvars64.bat failed. 1>&2
  exit /b 1
)

rem Fail here rather than inside the command, so the diagnostic names the cause.
where cl.exe >nul 2>nul
if errorlevel 1 (
  echo error: cl.exe still not on PATH after running vcvars64.bat. 1>&2
  exit /b 1
)

:run
call %*
exit /b %ERRORLEVEL%
