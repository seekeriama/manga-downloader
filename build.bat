@echo off
chcp 65001 >nul
echo Dang build "CBZ Downloader" (lan dau mat khoang 5-10 phut)...
cargo build --release
if errorlevel 1 (
  echo.
  echo BUILD LOI. Xem thong bao phia tren.
  pause
  exit /b 1
)
copy /Y target\release\cbz_downloader.exe cbz_downloader.exe >nul
echo.
echo XONG: da tao file cbz_downloader.exe trong thu muc nay. Bam dup chuot vao file do de chay.
pause
