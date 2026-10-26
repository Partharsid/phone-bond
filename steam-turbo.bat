@echo off
rem One click: Steam downloads through all your tethered phones.
rem Asks for admin (to point Steam's cache name at phone-bond), restarts Steam,
rem and puts everything back to normal when you press Ctrl+C.
net session >nul 2>&1 || (powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs" & exit /b)
title phone-bond (Steam)
cd /d "%~dp0"
node bond.js --steam --restart-steam %*
rem Safety net if bond crashed: never leave Steam pointed at a cache that isn't running.
node bond.js --off >nul 2>&1
pause
