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
rem .cargo/config.toml limits the test harness to four threads.
rem To isolate a rendering test that stalls, rerun with -- --test-threads=1.
cargo test --offline >> check_output.txt 2>&1
echo.
echo Finished. Results are in check_output.txt
pause
