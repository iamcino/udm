"use strict";
const api = globalThis.browser || globalThis.chrome;
const T = globalThis.UdmStrings.udmT();
const SETTINGS_KEY = "udmSettings";
const LOG_KEY = "udmHandoffLog";
const PENDING_KEY = "udmHandoffPending";

document.getElementById("title").textContent = T.title;
document.getElementById("enabledLabel").textContent = T.enabled;
document.getElementById("recentLabel").textContent = T.recent;
document.getElementById("optionsLink").textContent = T.options;

const enabledBox = document.getElementById("enabled");

// A stored reason is shown as written. A record without one gets the
// generic status wording — never a guessed cause such as a save prompt.
function statusLabel(item) {
  if (item.reason) return item.reason;
  switch (item.status) {
    case "pending":
      return T.status_pending;
    case "recovery-required":
      return T.status_recovery;
    case "discarded":
      return T.status_discarded;
    case "expired":
      return T.status_expired;
    case "aborted":
      return T.status_aborted;
    case "kept-by-browser":
      return T.status_kept;
    default:
      return T.status_attention;
  }
}

// Read-only view of the pending journal, plus attention rows from the
// shared log. Success rows are removed from the log when this page loads;
// the pending key and settings are not written here.
async function attentionLog(stored) {
  const migrated = globalThis.UdmHandoff.migrateHandoffLog(stored);
  if (!Array.isArray(stored) || !migrated || migrated.length === stored.length) return migrated || [];
  const again = await api.storage.local.get(LOG_KEY);
  const latest = again[LOG_KEY];
  const cleaned = Array.isArray(latest) ? globalThis.UdmHandoff.migrateHandoffLog(latest) : migrated;
  if (Array.isArray(latest) && cleaned && cleaned.length !== latest.length) {
    await api.storage.local.set({ [LOG_KEY]: cleaned });
  }
  return cleaned || [];
}

async function load() {
  const got = await api.storage.local.get([SETTINGS_KEY, LOG_KEY, PENDING_KEY]);
  const s = got[SETTINGS_KEY] || {};
  enabledBox.checked = s.enabled !== false;
  const log = await attentionLog(got[LOG_KEY]);
  const items = globalThis.UdmHandoff.projectPopupItems(got[PENDING_KEY], log);
  const ul = document.getElementById("log");
  ul.textContent = "";
  const none = document.getElementById("none");
  none.hidden = items.length > 0;
  if (items.length === 0) none.textContent = T.none;
  for (const item of items) {
    const li = document.createElement("li");
    if (item.needsAction) li.className = "needs-action";
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = item.name || "";
    name.title = item.name || "";
    const status = document.createElement("span");
    status.className = "status " + item.status + (item.needsAction ? " needs-action" : "");
    status.textContent = statusLabel(item);
    li.append(name, status);
    ul.append(li);
  }
}

enabledBox.addEventListener("change", async () => {
  const got = await api.storage.local.get(SETTINGS_KEY);
  const s = got[SETTINGS_KEY] || {};
  s.enabled = enabledBox.checked;
  await api.storage.local.set({ [SETTINGS_KEY]: s });
});

document.getElementById("optionsLink").addEventListener("click", (e) => {
  e.preventDefault();
  api.runtime.openOptionsPage();
});

load();
