<div align="center">

<img src="crates/gui/assets/icon-256.png" width="96" alt="UDM">

# Universal Download Manager

**Giữ các lượt tải luôn tiến bước với ứng dụng gốc dành cho Windows 11, do bạn làm chủ.**<br>
UDM chia tệp lớn thành các luồng tải đồng thời, lưu tiến độ có thể khôi phục trên ổ đĩa và gom mọi lượt tải vào một giao diện yên tĩnh. Không tài khoản, không telemetry, không cần quyền quản trị.

[English](README.md) · **Tiếng Việt**

[![Release](https://img.shields.io/gitlab/v/release/cino-group/udm?label=release&color=0a7aff)](https://gitlab.com/cino-group/udm/-/releases)
![Platform](https://img.shields.io/badge/platform-Windows%2011%20x64-0a7aff)
![Rust](https://img.shields.io/badge/Rust-stable-000000?logo=rust)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)](LICENSE)

### ⬇ Tải về: [bộ cài](https://gitlab.com/cino-group/udm/-/releases/permalink/latest/downloads/udm-setup.exe) · [bản portable](https://gitlab.com/cino-group/udm/-/releases/permalink/latest/downloads/udm-portable.zip)

[Tất cả phiên bản](https://gitlab.com/cino-group/udm/-/releases) · [Có gì mới](CHANGELOG.vi.md) · [Báo lỗi](https://gitlab.com/cino-group/udm/-/issues)

</div>

---

## Dùng ngay trong một phút

1. **Tải** `udm-<phiên bản>-setup.exe` [từ GitLab](https://gitlab.com/cino-group/udm/-/releases/permalink/latest/downloads/udm-setup.exe) và chạy. Bộ cài chỉ cài cho tài khoản Windows của bạn nên không hỏi quyền quản trị.
2. **Dán liên kết.** Sao chép liên kết HTTP hoặc HTTPS trực tiếp đến tệp, dán vào thanh công cụ rồi nhấn Enter.
3. **Theo dõi.** Danh sách hiển thị tiến độ, tốc độ, trạng thái và giao thức đang dùng. Tạm dừng, tiếp tục hoặc mở tệp đã tải xong ngay trên từng dòng.

Vậy là xong. Đóng cửa sổ thì UDM vẫn chạy dưới khay hệ thống (góc phải thanh
tác vụ) để tải tiếp; chọn **Thoát** trong menu khay để thoát hẳn. Nếu Windows
hiện cảnh báo, xem [Cảnh báo của Windows](#cảnh-báo-của-windows-và-phần-mềm-diệt-virus).

> Không muốn cài? Giải nén `udm-<phiên bản>-portable.zip` vào thư mục bất kỳ
> bạn có quyền ghi rồi chạy `udm.exe`. Dữ liệu nằm ngay cạnh chương trình.

## Vì sao chọn UDM

| | |
|---|---|
| 🚀 **Tải đồng thời** | Tệp lớn được tải thành nhiều khoảng byte cùng lúc, qua HTTP/1.1, HTTP/2 hoặc HTTP/3 (QUIC). Số lượt tải chạy cùng lúc có giới hạn riêng. |
| ↩️ **Tiếp tục tải an toàn** | Điểm lưu được ghi xuống đĩa và mỗi lần tải tiếp đều kiểm tra đúng là cùng một tệp, nên dữ liệu dở chỉ được dùng lại khi an toàn và không bao giờ trộn phiên bản. |
| ✅ **Checksum** | Nhập SHA-256, SHA-512, SHA-1, MD5 hoặc BLAKE3 của bên phát hành, UDM kiểm tra tệp trước khi báo hoàn tất. |
| 🗂️ **Tệp gọn gàng** | Tùy chọn phân loại đưa tệp vào thư mục con theo loại tệp. Mặc định tắt. |
| 🌐 **Nhận từ trình duyệt** | Tiện ích Chrome hoặc Firefox chuyển lượt tải cho ứng dụng đang chạy qua native messaging host cục bộ. Bạn chủ động bật. |
| 🔒 **Riêng tư, theo từng người dùng** | Không phân tích, quảng cáo, báo lỗi từ xa hay kiểm tra cập nhật. Không dịch vụ, không quyền quản trị; cài đặt và lịch sử nằm trong thư mục của bạn. |
| 🪶 **Nhẹ và gốc** | Một ứng dụng Rust với giao diện [Slint](https://slint.dev), kèm công cụ dòng lệnh dùng chung engine. |

## Tính năng

### Usagi: engine tải

- **Tải chia đoạn** qua HTTP/1.1, HTTP/2 và HTTP/3. UDM dùng HTTP/3 khi máy chủ
  hỗ trợ và chuyển giao thức khi chính sách truyền tải cho phép.
- **Tiến độ bền vững.** Điểm lưu được đồng bộ trước khi tính, tệp dở nằm cạnh
  đích với tên `<tên-tệp>.udm.part`.
- **Kiểm tra phiên bản tệp.** Chỉ dùng lại byte đã lưu khi máy chủ chứng minh tệp
  không đổi; nếu không, lượt tải bắt đầu lại thay vì tạo ra tệp hỏng.
- **Checksum** được kiểm tra trước khi lượt tải được đánh dấu hoàn tất.

### Ứng dụng

- Tiếng Anh hoặc tiếng Việt, giao diện Tối, Sáng hoặc Tự động (theo Windows) và
  nhiều màu nhấn.
- **Biểu tượng khay** hiển thị tổng trạng thái và cửa sổ nhỏ các lượt đang tải,
  tạm dừng hoặc tiếp tục ngay từng dòng.
- **Thêm lượt tải** để nhập nhiều liên kết, chọn thư mục, checksum hoặc tùy chọn
  riêng cho từng lượt.
- **Khởi động cùng Windows**, khởi động thu nhỏ và thu nhỏ xuống khay là các tùy
  chọn riêng, mặc định đều tắt. Khởi động cùng Windows chỉ ghi mục của UDM vào
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` và xóa mục đó khi tắt.

### Nhận lượt tải từ trình duyệt

Xem [hướng dẫn tiện ích](extension/README.md). Bộ cài và bản portable **chưa kèm**
`udm-host` hay tiện ích; cần build host riêng. Quy trình chuyển tải dùng bản ghi
prepare/commit bền vững; mục nào chưa khớp được với tác vụ sẽ hiện thành dòng chỉ
đọc **Cần khôi phục** trong mục **Tất cả**.

## Mẹo nhanh

| Để… | Làm thế này |
|---|---|
| Thêm nhiều liên kết cùng lúc | **Thêm lượt tải**, mỗi dòng một liên kết |
| Kiểm tra tệp theo mã của bên phát hành | **Thêm lượt tải** › checksum, hoặc `udm-cli get <url> --hash sha256:<digest>` |
| Xếp tệp vào thư mục theo loại | Bật phân loại trong cài đặt |
| Dùng mạng bắt buộc qua HTTP proxy | Đặt truyền tải thành **Không dùng HTTP/3** (QUIC dùng UDP, không qua proxy hệ thống) |
| Mở lại cửa sổ | Nhấp đúp biểu tượng khay |
| Thoát hẳn | Menu khay › **Thoát** |

## Tệp được lưu ở đâu?

| Chế độ | Dữ liệu ứng dụng |
|---|---|
| Đã cài, hoặc chỉ chạy tệp thực thi | `%LOCALAPPDATA%\udm` |
| Portable (`udm.portable` cạnh tệp thực thi, hoặc `UDM_PORTABLE=1`) | `data\` cạnh tệp thực thi |
| Chỉ định riêng | Thư mục trong `UDM_DATA_DIR` |

Tệp tải về nằm trong thư mục tải đã cấu hình, thường là Downloads của Windows.
Portable chỉ chuyển dữ liệu ứng dụng, **không** chuyển tệp tải về. Cơ sở dữ liệu là
`udm.redb`; log là `logs\udm.log`; chuyển tải từ trình duyệt lưu thêm tệp trong
`handoff\`. Đường dẫn và biến môi trường `quget` cũ vẫn dùng được khi nâng cấp. Đọc
[vận hành và sao lưu](docs/OPERATIONS.md) trước khi di chuyển dữ liệu.

## Dòng lệnh

`udm-cli` chạy cùng engine Usagi mà không cần giao diện:

```powershell
udm-cli get https://example.com/file.iso --segments 8
udm-cli probe https://example.com/file.iso
udm-cli import urls.txt
udm-cli list
udm-cli settings --list
udm-cli settings segments_per_task=12
udm-cli doctor
```

Chạy `udm-cli --help` để xem lệnh và `udm-cli <lệnh> --help` để xem tùy chọn. Ứng
dụng và CLI không mở cùng một cơ sở dữ liệu được, nên hãy thoát UDM từ khay trước
khi dùng CLI trên dữ liệu đó.

## Yêu cầu hệ thống

- **Windows 11 x64.**
- **Không cần quyền quản trị.** Bộ cài theo từng người dùng, vào
  `%LOCALAPPDATA%\Programs\udm`.

## Cập nhật

UDM không tự kiểm tra cập nhật; phiên bản này không có kết nối nền nào. Phiên bản
mới được đăng ở [trang phát hành](https://gitlab.com/cino-group/udm/-/releases).
Cài bản mới đè lên bản cũ; cài đặt, lịch sử và tệp tải về vẫn được giữ.

## Hỏi đáp

**UDM có thu thập dữ liệu không?**
Không. Không phân tích, quảng cáo, báo lỗi từ xa hay kiểm tra cập nhật. UDM chỉ kết
nối tới máy chủ của các lượt tải bạn tạo. Log và URL tải nằm trên máy bạn nhưng có
thể chứa thông tin nhạy cảm, hãy xem lại trước khi chia sẻ. Xem
[chính sách quyền riêng tư](docs/PRIVACY.md).

**Đây có phải ứng dụng torrent hay công cụ lấy video không?**
Không. UDM tải tệp từ liên kết HTTP và HTTPS. Chuyển tải từ trình duyệt chỉ gửi URL
cho UDM, không sao chép phiên đăng nhập của bạn.

**Sao có lượt tải lại bắt đầu lại từ đầu?**
UDM chỉ dùng lại byte đã lưu khi máy chủ chứng minh tệp không đổi. Chỉ có ETag yếu
hoặc ngày Last-Modified thì chưa đủ.

**Gỡ cài đặt thế nào?**
Tắt **Khởi động cùng Windows** và hủy đăng ký host trình duyệt nếu đã bật, thoát UDM
từ khay, rồi vào **Cài đặt › Ứng dụng › UDM › Gỡ cài đặt**. Trình gỡ giữ lại dữ liệu
ứng dụng và tệp tải về. Bản portable chỉ cần xóa sau các bước trên. Xem
[hướng dẫn gỡ bỏ](docs/OPERATIONS.md#removing-udm).

## Xử lý sự cố

### Cảnh báo của Windows và phần mềm diệt virus

Các bản phát hành UDM có chữ ký số, nhưng bằng chứng chỉ riêng của dự án chứ không
phải chứng chỉ thương mại, nên lần đầu Windows có thể hiện "Windows protected your
PC" hoặc "Unknown publisher". Bấm **More info › Run anyway**. Để chắc bản tải là thật:

- **Chỉ tải** từ [trang phát hành chính thức](https://gitlab.com/cino-group/udm/-/releases).
- **Kiểm tra chữ ký:** mở PowerShell trong thư mục chứa tệp và chạy
  `(Get-AuthenticodeSignature .\udm.exe).SignerCertificate.Thumbprint`.
  Kết quả phải là `9FEE47A048EB37ABD97751E89DD750F75A38C5D4`
  (chứng chỉ ở [docs/signing/UDM-codesign.cer](docs/signing/UDM-codesign.cer)).
- **Kiểm tra mã băm** với `SHA256SUMS.txt` của cùng bản phát hành.

### Thêm trợ giúp

[Vận hành](docs/OPERATIONS.md) hướng dẫn sao lưu, khôi phục sau khi ứng dụng bị
tắt đột ngột hoặc lượt tải bị gián đoạn, khôi phục chuyển tải trình duyệt và các
lỗi thường gặp.

## Giới hạn hiện tại

- Chưa có giới hạn tốc độ tổng, hẹn giờ hàng đợi, torrent hay phân giải liên kết
  media. Hàng đợi chỉ giới hạn số lượt tải chạy cùng lúc.
- Đoạn tải là yêu cầu đồng thời, không phải kết nối vật lý: HTTP/2 và HTTP/3 có
  thể ghép nhiều đoạn trên một kết nối.
- Phiên bản này chưa có trình cập nhật trong ứng dụng, chưa dùng IPv4 và IPv6 cùng
  lúc và chưa chia một lượt tải qua nhiều họ địa chỉ.

## Dành cho nhà phát triển

UDM viết bằng Rust với giao diện [Slint](https://slint.dev), chia thành các crate:
`usagi` (`crates/core`, engine), `udm-gui` (`crates/gui`, ứng dụng), `udm-cli`
(`crates/cli`) và `udm-host` (`crates/host`, native messaging host cho trình duyệt).

```
 Tiện ích trình duyệt ── Native Messaging ──► udm-host
                                                 │ named pipe theo người dùng
                                                 ▼
 udm.exe (giao diện Slint, khay) ──────► Engine Usagi ◄─── udm-cli.exe (khi CSDL rảnh)
                                                 │
                         ┌───────────────────────┼───────────────────────┐
                         ▼                       ▼                       ▼
                    HTTP/3 · HTTP/2          udm.redb            .udm.part → tệp hoàn tất
                    · HTTP/1.1
```

| Bạn muốn… | Tài liệu |
|---|---|
| Build ứng dụng, bộ cài và bản phát hành | [Hướng dẫn build](docs/BUILD.md) |
| Hiểu các crate, lưu trữ và IPC | [Kiến trúc](docs/ARCHITECTURE.md) |
| Theo dõi từ URL đến tệp hoàn tất | [Engine Usagi](docs/ENGINE.md) |
| Hiểu giao diện | [Thiết kế](docs/DESIGN.md) |
| Chạy, sao lưu hoặc xử lý sự cố UDM | [Vận hành](docs/OPERATIONS.md) |
| Tìm tiêu chuẩn và dự án nền tảng | [Tham khảo](docs/REFERENCES.md) |

## Giấy phép và ghi công

Copyright (c) 2026 UDM contributors

UDM là phần mềm tự do, mã nguồn mở, cấp phép kép [MIT OR Apache-2.0](LICENSE).
Giao diện dùng [Slint](https://slint.dev) theo Slint Royalty-free License 2.0,
[thư viện cino-ui đi kèm](crates/gui/vendor/cino-ui/LICENSE) và phông
[Be Vietnam Pro](crates/gui/vendor/cino-ui/ui/fonts/OFL.txt) (SIL OFL 1.1). Các thư
viện Rust giữ giấy phép riêng.

UDM được cung cấp "nguyên trạng", không kèm bảo đảm. Chỉ tải những tệp bạn có
quyền tải.
