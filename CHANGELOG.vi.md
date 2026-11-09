# Nhật ký thay đổi

Các thay đổi đáng chú ý của UDM. Tiếng Anh: [CHANGELOG.md](CHANGELOG.md).

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
