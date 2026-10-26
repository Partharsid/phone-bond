@echo off
rem Removes the phone-bond line from the hosts file, so Steam downloads directly again.
net session >nul 2>&1 || (echo Right-click this file and choose "Run as administrator". & pause & exit /b 1)
set HOSTS=%SystemRoot%\System32\drivers\etc\hosts
findstr /v /c:"lancache.steamcontent.com # phone-bond" "%HOSTS%" > "%TEMP%\hosts.phonebond"
copy /y "%TEMP%\hosts.phonebond" "%HOSTS%" >nul
del "%TEMP%\hosts.phonebond"
ipconfig /flushdns >nul
echo Done. Restart Steam to download directly again.
pause
