@echo off &setlocal
:: arg1 = string to replace
:: arg2 = replacement
:: arg3 = input file
:: arg4 = output file
:: Take all instances of arg1 in arg3 and replace it with arg2
:: The modified string is outputted into arg4, arg3 will not be modified
set BEFORE=%1
set AFTER=%2
set IN=%3
set OUT=%4
(for /f "delims=" %%i in (%IN%) do (
    set "line=%%i"
    setlocal enabledelayedexpansion
    set "line=!line:%BEFORE%=%AFTER%!"
    echo(!line!
    endlocal
))>"%OUT%"
