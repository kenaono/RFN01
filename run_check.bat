@echo off
chcp 65001 > nul
cd /d "D:\Projects\10_Creation\50_Dev\10_Editor"
echo ==== cargo fmt --check ==== > check_output.txt 2>&1
cargo fmt --check >> check_output.txt 2>&1
echo. >> check_output.txt
echo ==== cargo check --offline ==== >> check_output.txt 2>&1
cargo check --offline >> check_output.txt 2>&1
echo. >> check_output.txt
echo ==== cargo test --offline ==== >> check_output.txt 2>&1
rem One thread, because two of the tile-rendering tests stop responding when
rem the suite runs them beside everything else (see 7.3 of the validation
rem notes). The suite takes 52 seconds this way against 3 in parallel; a
rem filtered run is the quick way round that during development.
rem
rem Tests print as they finish, so a run stopped part way names the slow one
rem by omission: whatever is missing from the list is what it was waiting on.
cargo test --offline -- --test-threads=1 >> check_output.txt 2>&1
echo.
echo Finished. Results are in check_output.txt
pause
