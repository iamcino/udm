# Nhật ký thay đổi

Các thay đổi đáng chú ý của UDM. Tiếng Anh: [CHANGELOG.md](CHANGELOG.md).

## 0.3.1

- **Hộp thoại chuyển tải, hoàn thiện**: hộp thoại xác nhận lượt tải từ trình duyệt nay vừa nội dung, kéo giãn được, và tự nổi lên trước khi có lượt tải đến.
- **Một cửa sổ cho mỗi lượt tải**: bấm Tải sẽ mở cửa sổ chi tiết gọn cho lượt tải đó — tiến độ, tốc độ, và Tạm dừng / Tiếp tục / Hủy / Thử lại / Mở thư mục mà không cần quay về cửa sổ chính.
- **Làm mới cửa sổ chính**: thanh công cụ phía trên đưa Thêm lượt tải và các thao tác tác vụ ra trước; hai mục Thư viện và Danh mục ở thanh bên thu gọn được; icon nay là icon màu sống động.
- **Khay hệ thống**: click chuột trái vào icon khay mở cửa sổ chính (menu vẫn ở chuột phải).
- **Cài đặt gọn gàng hơn**: các trình duyệt được nhóm kèm icon và trạng thái, các ô nhập vừa nội dung, và nhóm Chrome có giải thích về tùy chọn "Hỏi nơi lưu mỗi file" — tùy chọn có thể khiến Chrome hiện hộp thoại Lưu thành trước khi UDM nhận lượt tải.
- **Tiện ích**: đổi tên thành "Universal Download Manager"; popup của tiện ích nay chỉ liệt kê các lượt tải cần chú ý.
- **Đã sửa**: UDM có thể thoát im lặng khi khởi động, và không bao giờ hiện hộp thoại xác nhận, khi còn một lượt chuyển tải từ trình duyệt đang chờ.

## 0.3.0

- **Danh mục trong cửa sổ chính**: thanh bên nay hiện các danh mục tải kèm số lượng của từng mục. Chọn một danh mục để lọc danh sách tác vụ, kết hợp được với bộ lọc trạng thái và tìm kiếm; có thêm chế độ xem đã hoàn thành và chưa hoàn thành.
- **Lượt tải từ trình duyệt nay hỏi trước**: khi tiện ích chuyển một lượt tải sang, UDM hiện hộp thoại xác nhận với tên file, dung lượng và nguồn tải. Bạn chọn thư mục lưu và danh mục, rồi tải ngay, thêm lượt tải ở trạng thái tạm dừng, hoặc bỏ hẳn. Nếu hộp thoại hết hạn mà chưa được trả lời, một thông báo trong cửa sổ chính giữ cho lượt tải không bị mất.

## 0.2.4

- **Kiểm tra cập nhật, đã sửa**: sau một lần kiểm tra thành công, kiểm tra lại không còn báo lỗi từ chối "manifest epoch". Kiểm tra nay là thao tác chỉ đọc thuần túy — kiểm tra lại bao nhiêu lần cũng cho cùng một đáp án, và "Đã là bản mới nhất" được báo đúng.
- **Bộ cập nhật viết lại theo hai host phát hành**: UDM nay hỏi GitHub và GitLab bản phát hành mới nhất song song và lấy bản mới hơn. Một bản cập nhật chỉ được chấp nhận khi SHA-256 của file nén khớp `SHA256SUMS.txt` đã công bố và mọi file chương trình mang chữ ký số hợp lệ của UDM.

## 0.2.3

- **Đóng xuống khay, đã sửa**: đóng cửa sổ khi bật "Đóng xuống khay" nay giữ UDM chạy trong khay; ứng dụng không còn tự thoát khi cửa sổ bị ẩn.
- **Thu nhỏ xuống khay, đã sửa**: thu nhỏ khi bật "Thu nhỏ xuống khay" không còn bật cửa sổ trở lại ngay.
- **Cài đặt từ dòng lệnh**: `udm-cli settings set` nay nhận `close_to_tray`, `minimize_to_tray`, `start_minimized` và `start_with_windows`.
- **Nhận lượt tải từ trình duyệt, chắc chắn hơn**: tiện ích hủy lượt tải của trình duyệt trước, nên hộp thoại Save As của Chrome không còn chặn việc bàn giao; lượt tải nào trình duyệt giữ lại sẽ được trình duyệt tiếp tục thay vì bị bỏ.
- **Khay hệ thống**: **Kiểm tra cập nhật** nay mở lại cửa sổ chính trước khi bắt đầu kiểm tra, nên kết quả hiển thị được.
- **Sửa căn chỉnh**: các nhãn ở trang Giới thiệu, biểu tượng và nhãn của nút thương hiệu, và các chấm trạng thái trình duyệt ở Cài đặt đã được căn giữa theo hàng.
- **Menu khay theo giao diện**: menu khay nay tối khi ứng dụng tối, sáng khi ứng dụng sáng.

## 0.2.2

- **Tải không còn chết vì nghẽn**: khi một kết nối của một lượt tải
  nhiều đoạn bị nghẽn, UDM thử lại đúng đoạn đó thay vì khởi động lại
  cả lượt, và hàng tác vụ hiển thị lần thử lại trong lúc nó diễn ra.
  Một lượt tải trước đây có thể đốt hết số lần thử vì vài kết nối
  nghẽn rồi thất bại, nay tự hồi phục.
- **Nhận lượt tải từ trình duyệt, chắc chắn hơn**: việc hủy phía
  Chrome được xác nhận trước khi quyền sở hữu chuyển giao, và lượt
  tải nào trình duyệt giữ lại sẽ được trình duyệt tiếp tục trên mọi
  đường thoát.
- **Tiện ích Firefox dưới dạng tệp cài được**: bộ cài và bản portable
  kèm tiện ích Firefox dưới dạng tệp zip trong thư mục `extension` —
  chọn tệp đó tại **Load Temporary Add-on**. Mọi tệp zip đóng gói nay
  ghi tên mục theo dạng portable, nên biểu tượng tiện ích hiển thị
  đúng trong Firefox.
- **Khay hệ thống**: thêm mục **Kiểm tra cập nhật** vào menu khay, và
  các liên kết GitHub/GitLab ở trang Giới thiệu có logo hãng.
- **Đã sửa**: cửa sổ tải đang chạy có thể mở vắt qua hai màn hình.
- **Đã sửa**: lỗi hết giờ từng báo "timed out after 0s"; nay hiển thị
  thời gian chờ đo thật.

## 0.2.1

- **Tiện ích trình duyệt nay đi kèm UDM**: bộ cài và bản portable mang
  theo tiện ích trong thư mục `extension` (hai thư mục con `chrome` và
  `firefox`), và mỗi bản phát hành cũng đính kèm tiện ích dưới dạng zip
  riêng. Hướng dẫn cài đặt trong Cài đặt và README đã được viết lại theo
  cách đóng gói mới.
- **Trang Giới thiệu gọn lại**: phiên bản thật, dòng tác giả và giấy
  phép, liên kết dự án (GitHub, GitLab) và nơi lưu dữ liệu. Các đoạn văn
  dài đã chuyển ra; chúng nằm trong README và tài liệu riêng tư.
- **Đổi tên thư mục dữ liệu**: thư mục dữ liệu người dùng không còn dùng
  mật danh tiền phát hành `quget`; các bản đã cài được tự di cư sang
  `udm` ở lần khởi động đầu tiên, toàn bộ dữ liệu (gồm cả trạng thái cập
  nhật) đi theo.
- **User-Agent luôn mới**: User-Agent còn ở mặc định gốc nay theo phiên
  bản hiện tại thay vì phiên bản lúc mới cài UDM; giá trị đã tuỳ chỉnh
  được giữ nguyên.
- **Đã sửa**: ghi chú cuối mục **Cài đặt → Trình duyệt** có thể bị cắt
  khi hiển thị.
- **Đã sửa**: tiện ích trên Chrome nhận nhầm Chrome hiện nay là Firefox
  nên gọi sai host native, khiến việc chuyển không bao giờ kết nối được.
  Trang tuỳ chọn của tiện ích nay hiển thị ID tiện ích và chỉ tới phần
  đăng ký trong **Cài đặt → Trình duyệt** thay cho lệnh terminal.

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
