# manga-downloader
Download manga/comic and convert it into cbz file for your offline reading.

**Trình tải truyện CBZ** viết bằng Rust, có giao diện desktop và giao diện trình duyệt web.

**Tải bản Windows:** vào [Releases](https://github.com/seekeriama/manga-downloader/releases) (bản ổn định) hoặc tab [Actions](https://github.com/seekeriama/manga-downloader/actions) → lần chạy mới nhất → Artifacts (bản mới nhất).

Phần mềm có giao diện để tải truyện tranh từ web về thành file `.cbz`.

## Tính năng
1. Dán địa chỉ trang truyện rồi bấm **Kiểm tra**. Phần mềm báo tải được hay không.
2. Nếu tải được: hiện danh sách chương. Có thể chọn/bỏ chọn từng chương, lọc theo tên, chọn theo khoảng số thứ tự.
3. Chọn cách lưu:
   - **Lưu từng chương** thành file riêng (trong thư mục mang tên truyện; chương đã có sẽ được bỏ qua, nên có thể tải tiếp phần còn thiếu).
   - **Gộp tất cả** chương đã chọn thành **1 file CBZ**, có bookmark ở đầu mỗi chương.
4. **Loại bỏ trang quảng cáo**: bỏ ảnh nhỏ hơn N KB (1 KB = 1024 byte), chọn xét *chỉ ảnh đầu và cuối mỗi chương* hoặc *mọi ảnh*. Có thể lưu ảnh bị loại vào thư mục `_anh_bi_loai` để kiểm tra lại.

## Bản mở bằng trình duyệt web
Cùng file `truyen_dl.exe` có thể chạy với giao diện trong trình duyệt (Chrome, Edge...):
- Bấm đúp **`Mo_ban_web.bat`** (đặt cạnh `truyen_dl.exe`), hoặc chạy `truyen_dl.exe --web`,
  hoặc bấm nút **🌐 Mở bản trình duyệt** trong giao diện thường.
- Trình duyệt tự mở trang `http://127.0.0.1:8787/`. Trang này chỉ máy của bạn truy cập được.
- Tính năng giống hệt bản thường; thư mục lưu và tùy chọn lọc quảng cáo được trình duyệt ghi nhớ.
- Dùng xong bấm **Tắt phần mềm** ở góc trên. Nếu lỡ đóng tab, chạy lại `Mo_ban_web.bat` sẽ mở lại đúng trang cũ.

Vì sao không phải trang web thuần? Trình duyệt chặn trang web đọc ảnh từ trang truyện khác (CORS),
không cho tự đặt header Referer mà truyenggvn yêu cầu, và không cho ghi file thẳng vào thư mục.
Nên phần tải vẫn do `truyen_dl.exe` làm, trình duyệt chỉ là giao diện.

## Trang được hỗ trợ
| Trang | Ghi chú |
|---|---|
| truyenggvn.com (và các trang cùng cấu trúc `/truyen-tranh/<tên>-<số>`) | Dán link trang truyện hoặc link một chương bất kỳ |
| Các trang chạy trên Blogger, vd. pokemonspecial.com | Dán link trang chủ (tất cả bài) hoặc link `/search/label/<nhãn>` (chỉ nhãn đó) |
| cuutruyen.net | **Không hỗ trợ**: ảnh bị xáo trộn bằng DRM |

## Cách build trên Windows (một lần)
1. Cài Rust: tải `rustup-init.exe` từ https://rustup.rs và chạy. Khi trình cài hỏi, đồng ý cài **Visual Studio Build Tools** (gói "Desktop development with C++").
2. Tải mã nguồn (nút **Code → Download ZIP** trên GitHub) và giải nén, ví dụ vào `Downloads`.
3. Bấm đúp **`build.bat`** (hoặc mở PowerShell trong thư mục và chạy `cargo build --release`).
   Lần đầu mất khoảng 5–10 phút vì phải tải và biên dịch thư viện.
4. Xong sẽ có file **`truyen_dl.exe`** trong thư mục. Bấm đúp để chạy. File `.exe` này có thể copy đi chỗ khác dùng độc lập.

## Mẹo
- Có thể mở kèm địa chỉ: `truyen_dl.exe https://truyenggvn.com/truyen-tranh/hanh-trinh-cua-uduchi-5804`
- Ngưỡng quảng cáo đã dùng trước đây: truyenggvn (Uduchi) là ảnh đầu/cuối < 100 KB.
- Nên bật "Lưu ảnh bị loại..." ở lần đầu với trang mới để chắc chắn không loại nhầm trang truyện.
- Bấm **Dừng** khi đang tải: file gộp dở dang sẽ bị xoá, không ảnh hưởng file đã có.

## Build tự động trên GitHub (không cần cài Rust)
Thư mục `.github/workflows/build.yml` giúp GitHub tự build file `.exe` cho Windows.

1. Mỗi lần có thay đổi được đẩy lên nhánh `main`, GitHub tự build.
2. Mở tab **Actions** trên GitHub, đợi khoảng 5–10 phút. Bấm vào lần chạy mới nhất, phần **Artifacts** có file `truyen_dl-windows` để tải về.
3. Muốn có bản phát hành (Release) cố định để chia sẻ link, đẩy một tag phiên bản:
   ```powershell
   git tag v1.0.0
   git push origin v1.0.0
   ```
   Sau khi build xong, tab **Releases** sẽ có file `truyen_dl-windows.zip`.


## Giấy phép
MIT, xem file [LICENSE](LICENSE).
