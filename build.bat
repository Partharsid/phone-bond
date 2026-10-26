@echo off
rem Builds phone-bond.exe: Node's single-executable feature, with bond.js and ui.html inside.
rem Needs Node 20+ and internet once (for postject). Output: dist\phone-bond.exe
cd /d "%~dp0"
if not exist dist mkdir dist
node --experimental-sea-config sea-config.json || exit /b 1
copy /y "%ProgramFiles%\nodejs\node.exe" dist\phone-bond.exe >nul 2>&1 || node -e "require('fs').copyFileSync(process.execPath,'dist/phone-bond.exe')"
call npx --yes postject@1.0.0-alpha.6 dist\phone-bond.exe NODE_SEA_BLOB dist\sea-prep.blob --sentinel-fuse NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2 --overwrite || exit /b 1
del dist\sea-prep.blob
echo Built dist\phone-bond.exe
