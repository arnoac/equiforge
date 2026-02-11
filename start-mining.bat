@echo off
equiforge-windows-x64.exe init
equiforge-windows-x64.exe node --mine
pause
```

Double-clicking that batch file will initialize and start mining. The `pause` keeps the window open if it crashes.

For the GitHub release, tell people:
```
1. Download equiforge-windows-x64.exe and start-mining.bat
2. Put both in the same folder
3. Double-click start-mining.bat
```

Or for power users:
```
Open PowerShell, cd to the folder, then:
  .\equiforge-windows-x64.exe init
  .\equiforge-windows-x64.exe node --mine