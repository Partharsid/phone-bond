@echo off
title phone-bond
cd /d "%~dp0"
node bond.js --system-proxy %*
rem If the window was closed abnormally, make sure the proxy is not left on.
node bond.js --off >nul 2>&1
pause
