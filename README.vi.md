<div align="center">

<img src="crates/gui/assets/icon-256.png" width="96" alt="Biểu tượng thỏ UDM">

# Universal Download Manager

**Giữ các lượt tải luôn tiến bước với ứng dụng gốc dành cho Windows 11, do bạn làm chủ.**<br>
UDM chia tệp lớn thành các luồng tải đồng thời, lưu tiến độ có thể khôi phục trên ổ đĩa và tập hợp các lượt tải trong một giao diện yên tĩnh.

[English](README.md) · **Tiếng Việt**

Rust · Slint · Windows 11 x64 · [MIT OR Apache-2.0](LICENSE)

[Bắt đầu](#bắt-đầu) · [Kết nối trình duyệt](extension/README.md) · [Build từ mã nguồn](docs/BUILD.md) · [Quyền riêng tư](docs/PRIVACY.md)

</div>

---

## UDM làm được gì?

UDM tải tệp từ liên kết HTTP và HTTPS. Engine **Usagi** (gói Rust `usagi`)
hỗ trợ tải chia đoạn qua HTTP/1.1, HTTP/2 và HTTP/3 trên QUIC. UDM dùng
HTTP/3 khi có thể và chuyển sang giao thức khác nếu chính sách truyền tải
đã chọn cho phép.

UDM là trình quản lý tải tệp, không phải ứng dụng torrent hay công cụ lấy
liên kết video từ trang web. Tính năng chuyển tải từ trình duyệt chỉ gửi
URL cho UDM, không sao chép phiên đăng nhập của bạn.

Phiên bản mã nguồn hiện tại là **0.1.0**. README này mô tả mã tại `241f550`,
không phải thông báo phát hành công khai hay cung cấp bản tải xuống.
Windows 11 x64 là nền tảng đích cho ứng dụng desktop. Các đường dẫn dành
cho Linux và macOS phục vụ phát triển và kiểm thử; tích hợp Windows chỉ
hoạt động trên nền tảng tương ứng.

## Vì sao chọn UDM

| Bạn có gì | Ý nghĩa khi sử dụng |
|---|---|
| **Tải đồng thời** | Tệp lớn có thể dùng nhiều yêu cầu tải theo khoảng byte; số lượt tải chạy cùng lúc được giới hạn riêng. Tốc độ thực tế vẫn phụ thuộc máy chủ và đường truyền. |
| **Tiếp tục tải thận trọng** | Mốc tiến độ đã đồng bộ xuống đĩa và kiểm tra phiên bản tệp giúp UDM dùng lại dữ liệu dở dang đủ điều kiện, tránh ghép các phiên bản khác nhau. |
| **Mã kiểm tra tuỳ chọn** | Cung cấp checksum của bên phát hành để kiểm tra tệp trước khi hoàn tất: SHA-256, SHA-512, SHA-1, MD5 hoặc BLAKE3. |
| **Sắp xếp tệp theo loại** | Tuỳ chọn phân loại đưa tệp vào thư mục con theo hậu tố tên tệp. Mặc định tắt. |
| **Khay hệ thống hữu ích** | Xem tổng trạng thái trực tiếp, mở cửa sổ nhỏ liệt kê lượt tải, tạm dừng hoặc tiếp tục từng dòng và trở về cửa sổ chính. |
| **Giao diện theo ý bạn** | Tiếng Anh hoặc tiếng Việt, giao diện Tối/Sáng/Tự động và các màu nhấn. Tự động theo chế độ ứng dụng của Windows. |
| **Chuyển tải từ trình duyệt** | Tiện ích Chrome hoặc Firefox chuyển lượt tải đủ điều kiện cho ứng dụng đang chạy qua native messaging host cục bộ. Bạn chủ động thiết lập tính năng này. |
| **Riêng tư, theo từng người dùng** | Không telemetry, không cần quyền quản trị, không dịch vụ hệ thống. Cài đặt và lịch sử nằm trong thư mục người dùng. Phiên bản này không kiểm tra cập nhật. |

## Bắt đầu

1. **Build UDM** theo [hướng dẫn Windows](docs/BUILD.md), hoặc dùng bản build
   được cung cấp cho bạn. Script đóng gói tạo trình cài đặt theo người dùng
   và tệp ZIP portable.
2. **Chạy `udm.exe`.** Dán liên kết HTTP hoặc HTTPS trực tiếp đến tệp vào
   thanh công cụ rồi nhấn Enter. Dùng **Thêm lượt tải** để nhập nhiều liên
   kết, chọn thư mục, checksum hoặc tuỳ chọn riêng cho lượt tải.
3. **Theo dõi tiến độ.** Danh sách hiển thị tiến độ, tốc độ, trạng thái và
   giao thức đang dùng. Tạm dừng hoặc tiếp tục bằng nút trên từng dòng;
   mở tệp đã tải xong hoặc thư mục chứa tệp từ danh sách.

Mặc định, đóng cửa sổ sẽ ẩn UDM xuống khay để tiếp tục tải. Nhấp đúp biểu
tượng khay để mở lại; chọn **Thoát** trong menu khay để thoát hẳn. Thu nhỏ
xuống khay, khởi động thu nhỏ và khởi động cùng Windows là các tuỳ chọn
riêng, đều mặc định tắt.

**Khởi động cùng Windows** là tuỳ chọn theo người dùng, có giải thích ngay
trong ứng dụng. Bật sẽ ghi mục UDM vào
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`; tắt sẽ xoá mục đó.
Hãy giữ tệp thực thi ở một thư mục ổn định.

Để nhận lượt tải từ trình duyệt, xem [hướng dẫn tiện ích](extension/README.md).
Trình cài đặt và ZIP portable hiện **chưa kèm** `udm-host` hay tiện ích;
cần build host riêng. Quy trình chuyển tải dùng bản ghi prepare/commit
bền vững. Cam kết chưa liên kết được với tác vụ hiện thành dòng chỉ đọc
**Cần khôi phục** trong mục **Tất cả**; đây không phải lượt tải thông
thường có nút Thử lại.

## Tệp được lưu ở đâu?

| Chế độ | Dữ liệu ứng dụng |
|---|---|
| Cài đặt theo người dùng hoặc chỉ chạy tệp thực thi | `%LOCALAPPDATA%\udm` |
| Portable (`udm.portable` cạnh tệp thực thi, hoặc `UDM_PORTABLE=1`) | `data\` cạnh tệp thực thi |
| Chỉ định riêng | Thư mục trong `UDM_DATA_DIR` |

Tệp tải xuống nằm trong thư mục tải đã cấu hình, thường là Downloads của
Windows. Portable thay đổi vị trí dữ liệu ứng dụng, **không tự chuyển tệp
tải xuống về cạnh tệp thực thi**. Tệp dở dang nằm tại thư mục đích với tên
`<tên-tệp>.udm.part`.

Cơ sở dữ liệu là `udm.redb`; log desktop là `logs\udm.log`. Tính năng
chuyển tải trình duyệt lưu thêm tệp cục bộ trong `handoff\`. Đường dẫn và
tên biến môi trường `quget` cũ vẫn được hỗ trợ khi nâng cấp. Đọc
[vận hành và sao lưu](docs/OPERATIONS.md) trước khi di chuyển dữ liệu cũ.

## Dùng dòng lệnh

`udm-cli` chạy cùng engine Usagi mà không cần giao diện desktop:

```powershell
udm-cli get https://example.com/file.iso --segments 8
udm-cli probe https://example.com/file.iso
udm-cli import urls.txt
udm-cli list
udm-cli settings --list
udm-cli settings segments_per_task=12
udm-cli doctor
```

Thay URL ví dụ bằng liên kết tải thật. Dùng `get --hash sha256:<digest>`
với đầy đủ mã do bên phát hành cung cấp nếu có. Chạy `udm-cli --help` để
xem lệnh và `udm-cli <lệnh> --help` để xem tuỳ chọn.

GUI và CLI không thể mở cùng một cơ sở dữ liệu đồng thời. Thoát UDM từ
khay trước khi dùng CLI trên dữ liệu đó. IPC trình duyệt chỉ phục vụ
chuyển lượt tải, không phải giao diện điều khiển CLI từ xa.

## Giới hạn và hướng phát triển

- Tiếp tục tải phụ thuộc bằng chứng từ máy chủ. ETag yếu hoặc chỉ có
  Last-Modified không đủ để dùng lại byte đã lưu; một số lượt tải sẽ bắt
  đầu lại từ đầu.
- QUIC dùng UDP và không đi qua HTTP proxy hệ thống. Chọn **Không dùng
  HTTP/3** trên mạng bắt buộc mọi lưu lượng phải qua HTTP proxy.
- Đoạn tải là yêu cầu đồng thời, không phải số kết nối vật lý: HTTP/2 và
  HTTP/3 có thể ghép nhiều luồng trên một kết nối. Phiên bản này chưa cung
  cấp tổng số kết nối của engine.
- Chưa có giới hạn tốc độ tổng, hẹn giờ hàng đợi, torrent hay phân giải
  liên kết media. Hàng đợi hiện chỉ giới hạn số lượt tải đồng thời.
- Trình cập nhật có xác thực, N2 dual-stack và N3 đa họ địa chỉ đều
  **chỉ là thiết kế, chưa được xây dựng**. Phiên bản này không có trình cập
  nhật trong ứng dụng, không dùng IPv4 và IPv6 cùng lúc và không chia một
  lượt tải qua các họ địa chỉ. Việc triển khai N2/N3 vẫn cần kết quả đo.

## Quyền riêng tư và gỡ bỏ

UDM không có phân tích sử dụng, quảng cáo hay báo cáo lỗi từ xa. Ứng dụng
kết nối đến máy chủ để thực hiện lượt tải bạn tạo, thăm dò, chuyển hướng
và thiết lập truyền tải cần thiết. Quản lý tác vụ và lịch sử cục bộ hoạt
động ngoại tuyến. Không có kiểm tra phiên bản nền. Log và URL tải vẫn có
thể chứa thông tin nhạy cảm; hãy xem lại trước khi chia sẻ. Đọc
[chính sách quyền riêng tư](docs/PRIVACY.md) để biết dữ liệu lưu và đăng ký
tích hợp hệ thống.

Trước khi gỡ UDM, tắt **Khởi động cùng Windows** và huỷ đăng ký host trình
duyệt đã bật. Sau đó thoát ứng dụng. Trình gỡ cài đặt xoá ứng dụng đã cài
và mục gỡ cài đặt, giữ lại dữ liệu ứng dụng và tệp tải. Có thể xoá bản
portable sau khi hoàn tác các đăng ký tuỳ chọn tương tự. Xem
[hướng dẫn gỡ bỏ](docs/OPERATIONS.md#removing-udm).

## Đọc tiếp

| Bạn muốn… | Tài liệu |
|---|---|
| Chạy, sao lưu hoặc xử lý sự cố UDM | [Vận hành](docs/OPERATIONS.md) |
| Chuyển lượt tải từ trình duyệt | [Tiện ích trình duyệt](extension/README.md) |
| Build ứng dụng và hiểu cách đóng gói | [Hướng dẫn build](docs/BUILD.md) |
| Hiểu các crate, lưu trữ và IPC | [Kiến trúc](docs/ARCHITECTURE.md) |
| Theo dõi từ URL đến tệp hoàn tất | [Engine Usagi](docs/ENGINE.md) |
| Hiểu giao diện và lựa chọn tương tác | [Thiết kế](docs/DESIGN.md) |
| Kiểm tra cam kết riêng tư | [Quyền riêng tư](docs/PRIVACY.md) |
| Tìm tiêu chuẩn và dự án nền tảng | [Tham khảo](docs/REFERENCES.md) |

## Giấy phép và ghi công

UDM được cấp phép kép [MIT OR Apache-2.0](LICENSE).

Giao diện dùng [Slint](https://slint.dev) theo Slint Royalty-free License
2.0, [thư viện cino-ui đi kèm](crates/gui/vendor/cino-ui/LICENSE) và
[Be Vietnam Pro](crates/gui/vendor/cino-ui/ui/fonts/OFL.txt). Các thư viện
Rust giữ giấy phép riêng tương ứng.
