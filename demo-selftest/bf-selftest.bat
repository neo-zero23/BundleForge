@echo off
rem bf-selftest: integrity probe for BundleForge test packages (Windows).
rem After installing, run it from the install dir: bf-selftest.bat
echo bf-selftest 1.0 on Windows
if exist "%~dp0hello.txt" (echo PASS: payload present) else (echo FAIL: payload missing & exit /b 1)
findstr /c:"hello-from-bundleforge" "%~dp0hello.txt" >nul && (echo PASS: content intact) || (echo FAIL: content changed & exit /b 1)
echo ALL PASS
