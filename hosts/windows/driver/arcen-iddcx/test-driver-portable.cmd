@echo off
setlocal EnableExtensions
where cl.exe >nul 2>nul
if errorlevel 1 (
  echo error: cl.exe is required for portable IddCx contract tests.
  exit /b 1
)
set "ROOT=%~dp0..\..\..\.."
for %%I in ("%ROOT%") do set "ROOT=%%~fI"
set "OUT=%ROOT%\target\arcen-iddcx-portable-test"
if exist "%OUT%" rmdir /s /q "%OUT%"
mkdir "%OUT%"
if errorlevel 1 exit /b %errorlevel%
cl.exe /nologo /std:c++17 /EHsc /W4 /WX ^
  /Fo"%OUT%\\" ^
  "%~dp0arcen_iddcx_model.cpp" ^
  "%~dp0arcen_iddcx_model_test.cpp" ^
  /Fe:"%OUT%\arcen-iddcx-model-test.exe"
if errorlevel 1 (
  rmdir /s /q "%OUT%" >nul 2>nul
  exit /b 1
)
"%OUT%\arcen-iddcx-model-test.exe"
set "RESULT=%ERRORLEVEL%"
rmdir /s /q "%OUT%" >nul 2>nul
exit /b %RESULT%
