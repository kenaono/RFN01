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
rem In parallel, which takes 3 seconds against the 52 this used to take.
rem It ran with --test-threads=1 until 2026-08-25 because two of the
rem tile-rendering tests stopped responding beside the rest of the suite.
rem That was the shared DirectWrite factory, the one COM object the whole
rem process shared; the factories are isolated now (see 7.3 of the
rem validation notes).
rem
rem If tests ever stop returning again, put --test-threads=1 back to get a
rem readable run: tests print as they finish, so whatever is missing from
rem the list is what it was waiting on.
cargo test --offline >> check_output.txt 2>&1
echo.
echo Finished. Results are in check_output.txt
pause
