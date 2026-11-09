"use strict";
const api = globalThis.browser || globalThis.chrome;
const T = globalThis.UdmStrings.udmT();
const SETTINGS_KEY = "udmSettings";
const LOG_KEY = "udmHandoffLog";

document.getElementById("title").textContent = T.title;
document.getElementById("enabledLabel").textContent = T.enabled;
document.getElementById("recentLabel").textContent = T.recent;
document.getElementById("optionsLink").textContent = T.options;

const enabledBox = document.getElementById("enabled");

async function load() {
  const got = await api.storage.local.get([SETTINGS_KEY, LOG_KEY]);
  const s = got[SETTINGS_KEY] || {};
  enabledBox.checked = s.enabled !== false;
  const log = Array.isArray(got[LOG_KEY]) ? got[LOG_KEY] : [];
  const ul = document.getElementById("log");
  ul.textContent = "";
  document.getElementById("none").hidden = log.length > 0;
  if (log.length === 0) document.getElementById("none").textContent = T.none;
  for (const entry of log) {
    const li = document.createElement("li");
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = entry.name || "";
    name.title = entry.name || "";
    const status = document.createElement("span");
    status.className = "status " + entry.status;
    status.textContent =
      entry.status === "handed-off"
        ? T.status_handed_off + (entry.task_ref ? ` #${entry.task_ref}` : "")
        : entry.status === "recovery-required"
          ? T.status_recovery
          : T.status_kept;
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
