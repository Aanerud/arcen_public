@echo off
setlocal EnableExtensions
where cl.exe >nul 2>nul
if errorlevel 1 (
  echo error: cl.exe is required for portable driver ring tests.
  exit /b 1
)
set "OUT=%TEMP%\arcen-microphone-driver-test-%RANDOM%-%RANDOM%"
mkdir "%OUT%"
if errorlevel 1 exit /b %errorlevel%
cl.exe /nologo /std:c++17 /EHsc /W4 /WX /DARCEN_MICROPHONE_PORTABLE_TEST ^
  /Fo"%OUT%\\" ^
  "%~dp0arcen_microphone_ring.cpp" ^
  "%~dp0arcen_microphone_ring_test.cpp" ^
  /Fe:"%OUT%\arcen-microphone-ring-test.exe"
if errorlevel 1 (
  rmdir /s /q "%OUT%" >nul 2>nul
  exit /b 1
)
"%OUT%\arcen-microphone-ring-test.exe"
set "RESULT=%ERRORLEVEL%"
rmdir /s /q "%OUT%" >nul 2>nul
exit /b %RESULT%
