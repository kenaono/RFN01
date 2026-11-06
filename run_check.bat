@echo off
setlocal
pushd "%~dp0" || exit /b 1
set "log=%CD%\check_output.txt"
> "%log%" echo Checkout: "%CD%"
>> "%log%" echo Scope: Windows normal suite. Privileged, ignored, WSL and live UI cases are separate.

echo Checking the Windows normal suite in "%CD%"
call :run git --version || goto :failed
call :run git rev-parse HEAD || goto :failed
call :run cargo fmt --check || goto :failed
call :run cargo check --locked --offline || goto :failed
call :run cargo test --locked --offline || goto :failed
call :run cargo test --locked --offline -- --ignored --list || goto :failed

>> "%log%" echo Result: passed
echo Passed. Results are in "%log%".
popd
exit /b 0

:failed
set "result=%errorlevel%"
>> "%log%" echo Result: failed ^(exit %result%^)
echo Failed ^(exit %result%^). Results are in "%log%".
popd
exit /b %result%

:run
>> "%log%" echo ==== %* ====
call %* >> "%log%" 2>&1
exit /b %errorlevel%
