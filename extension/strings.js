"use strict";
/* Universal Download Manager - popup/options strings (the app ships EN/VI). */
const UDM_STRINGS = {
  en: {
    title: "Universal Download Manager",
    enabled: "Auto-capture downloads",
    enabledHint: "When on, new browser downloads are handed to UDM. When UDM is not running, the browser keeps the download.",
    recent: "Needs attention",
    none: "Nothing needs attention.",
    options: "Options",
    exclusions: "Exclusions (one per line)",
    exclusionsHint: "Host (example.com), host suffix (*.example.com), file extension (.iso), or any text contained in the URL.",
    save: "Save",
    saved: "Saved.",
    registration: "Registration",
    registrationHintChrome: "Registration happens in UDM, not in a terminal: open UDM Settings, go to Browser integration, paste this extension's ID (shown below) into the Chrome ID field, then turn the Chrome switch on.",
    registrationHintFirefox: "Registration happens in UDM, not in a terminal: open UDM Settings, go to Browser integration, and turn the Firefox switch on. This extension's ID is fixed (shown below), so there is no field to fill.",
    status_handed_off: "Handed to UDM",
    status_pending: "In progress",
    status_attention: "Needs attention",
    status_kept: "Kept by browser",
    status_recovery: "Needs recovery in UDM",
    status_discarded: "Discarded — not downloaded",
    status_expired: "Handoff expired",
    status_aborted: "Aborted",
  },
  vi: {
    title: "Universal Download Manager",
    enabled: "Tự động chuyển tải xuống",
    enabledHint: "Khi bật, các lượt tải mới trong trình duyệt sẽ được chuyển cho UDM. Khi UDM không chạy, trình duyệt giữ nguyên lượt tải.",
    recent: "Cần chú ý",
    none: "Không có mục nào cần chú ý.",
    options: "Tùy chọn",
    exclusions: "Loại trừ (mỗi dòng một mục)",
    exclusionsHint: "Tên host (example.com), hậu tố host (*.example.com), đuôi tệp (.iso), hoặc bất kỳ đoạn văn bản nào có trong URL.",
    save: "Lưu",
    saved: "Đã lưu.",
    registration: "Đăng ký",
    registrationHintChrome: "Việc đăng ký thực hiện trong UDM, không phải ở terminal: mở Cài đặt trong UDM, vào mục Tích hợp trình duyệt, dán ID của tiện ích này (hiển thị bên dưới) vào ô ID Chrome rồi bật công tắc Chrome.",
    registrationHintFirefox: "Việc đăng ký thực hiện trong UDM, không phải ở terminal: mở Cài đặt trong UDM, vào mục Tích hợp trình duyệt và bật công tắc Firefox. ID của tiện ích này là cố định (hiển thị bên dưới) nên không có ô nào cần điền.",
    status_handed_off: "Đã chuyển cho UDM",
    status_pending: "Đang xử lý",
    status_attention: "Cần chú ý",
    status_kept: "Trình duyệt giữ lại",
    status_recovery: "Cần khôi phục trong UDM",
    status_discarded: "Đã bỏ — không tải",
    status_expired: "Lượt chuyển đã hết hạn",
    status_aborted: "Đã hủy giữa chừng",
  },
};
function udmLang() {
  const l = (navigator.language || "en").toLowerCase();
  return l.startsWith("vi") ? "vi" : "en";
}
function udmT() {
  return UDM_STRINGS[udmLang()];
}
if (typeof globalThis !== "undefined") {
  globalThis.UdmStrings = { UDM_STRINGS, udmLang, udmT };
}
