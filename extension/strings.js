"use strict";
/* UDM Handoff - popup/options strings (the app ships EN/VI). */
const UDM_STRINGS = {
  en: {
    title: "UDM Handoff",
    enabled: "Auto-capture downloads",
    enabledHint: "When on, new browser downloads are handed to UDM. When UDM is not running, the browser keeps the download.",
    recent: "Recent handoffs",
    none: "No handoffs yet.",
    options: "Options",
    exclusions: "Exclusions (one per line)",
    exclusionsHint: "Host (example.com), host suffix (*.example.com), file extension (.iso), or any text contained in the URL.",
    save: "Save",
    saved: "Saved.",
    registration: "Registration",
    registrationHint: "The native host must be registered once, from a terminal, before handoffs can reach UDM:",
    status_handed_off: "Handed to UDM",
    status_kept: "Kept by browser",
    status_recovery: "Needs recovery in UDM",
  },
  vi: {
    title: "UDM Handoff",
    enabled: "Tự động chuyển tải xuống",
    enabledHint: "Khi bật, các lượt tải mới trong trình duyệt sẽ được chuyển cho UDM. Khi UDM không chạy, trình duyệt giữ nguyên lượt tải.",
    recent: "Các lượt chuyển gần đây",
    none: "Chưa có lượt chuyển nào.",
    options: "Tùy chọn",
    exclusions: "Loại trừ (mỗi dòng một mục)",
    exclusionsHint: "Tên host (example.com), hậu tố host (*.example.com), đuôi tệp (.iso), hoặc bất kỳ đoạn văn bản nào có trong URL.",
    save: "Lưu",
    saved: "Đã lưu.",
    registration: "Đăng ký",
    registrationHint: "Host native cần được đăng ký một lần, từ terminal, trước khi việc chuyển có thể tới được UDM:",
    status_handed_off: "Đã chuyển cho UDM",
    status_kept: "Trình duyệt giữ lại",
    status_recovery: "Cần khôi phục trong UDM",
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
