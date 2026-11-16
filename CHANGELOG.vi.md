# Nhật ký thay đổi

Các thay đổi đáng chú ý của UDM. Tiếng Anh: [CHANGELOG.md](CHANGELOG.md).

## 0.2.0

- **Nhận lượt tải từ trình duyệt, tích hợp sẵn**: bộ cài và bản portable nay
  kèm `udm-host.exe`, và mỗi trình duyệt được bật từ **Cài đặt → Trình duyệt**
  (Chrome và Firefox). Nạp tiện ích thủ công (xem hướng dẫn cài tiện ích),
  bật công tắc, và các lượt tải bạn bắt đầu trong trình duyệt sẽ được chuyển
  cho UDM; tắt công tắc thì trình duyệt tự giữ lượt tải của mình. Hàng cài
  đặt trình duyệt hiển thị lần kết nối gần nhất của trình duyệt.
- **Tự động kiểm tra cập nhật**: UDM nay kiểm tra cập nhật lúc khởi động và
  mỗi 6 giờ (mặc định bật; tắt tại **Cài đặt → Bật kiểm tra cập nhật**). Bản
  cập nhật đã sẵn sàng sẽ hiện huy hiệu trên biểu tượng khay. Việc áp dụng
  cập nhật vẫn luôn là thao tác **Áp dụng** chủ động của bạn.
- **Xác thực cập nhật chặt hơn**: manifest cập nhật phải qua kiểm tra nhà
  phát hành và phiên bản nghiêm ngặt hơn trước khi được xếp chờ, nên
  manifest cũ hoặc bị thay thế sẽ bị từ chối thay vì được chào cài.

## 0.1.0

Bản phát hành công khai đầu tiên của Universal Download Manager cho Windows 11 x64.

- **Engine tải Usagi**: tải chia đoạn qua HTTP và HTTPS trên HTTP/1.1, HTTP/2 và HTTP/3
  (QUIC); dùng HTTP/3 khi có thể và chuyển giao thức khi chính sách truyền tải cho phép.
- **Tiếp tục tải an toàn**: điểm lưu được đồng bộ xuống đĩa và kiểm tra phiên bản tệp, nên dữ
  liệu tải dở chỉ được dùng lại khi đúng là cùng một tệp.
- **Kiểm tra checksum tùy chọn**: SHA-256, SHA-512, SHA-1, MD5 hoặc BLAKE3, kiểm tra trước khi
  hoàn tất.
- **Phân loại thư mục** (mặc định tắt) xếp tệp tải vào thư mục con theo loại tệp.
- **Biểu tượng khay hệ thống** hiển thị tổng tốc độ và cửa sổ nhỏ các lượt đang tải; đóng cửa
  sổ thì UDM vẫn tải tiếp dưới khay.
- **Tiếng Anh và tiếng Việt**, giao diện Tối/Sáng/Tự động và nhiều màu nhấn.
- **Khởi động cùng Windows** (tùy chọn), chỉ ghi vào khóa Run của người dùng hiện tại.
- **Nhận lượt tải từ trình duyệt** Chrome và Firefox qua native messaging host cục bộ. Bản này
  chưa kèm host và tiện ích mở rộng; xem docs/BUILD.md.
- **Bộ cài cho từng người dùng và bản portable ZIP**: không cần quyền quản trị, không dịch vụ,
  không telemetry và không tự kiểm tra cập nhật.
- File chạy và bộ cài được ký bằng chứng chỉ ký mã self-signed của UDM.
