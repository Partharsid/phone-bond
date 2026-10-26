@echo off
rem Points Steam's "lancache" name at phone-bond so Steam sends game downloads
rem through it. Needs admin (it edits the hosts file). Undo with steam-off.bat.
net session >nul 2>&1 || (echo Right-click this file and choose "Run as administrator". & pause & exit /b 1)
set HOSTS=%SystemRoot%\System32\drivers\etc\hosts
findstr /c:"lancache.steamcontent.com" "%HOSTS%" >nul || (echo.& echo 127.0.0.1 lancache.steamcontent.com # phone-bond) >> "%HOSTS%"
ipconfig /flushdns >nul
echo Done. Start phone-bond with: node bond.js --steam
echo Then fully exit Steam (tray icon - Exit) and open it again.
pause
