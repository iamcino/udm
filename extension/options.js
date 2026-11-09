"use strict";
const api = globalThis.browser || globalThis.chrome;
const T = globalThis.UdmStrings.udmT();
const SETTINGS_KEY = "udmSettings";
const isFirefox = !!globalThis.browser;

document.title = `${T.title} - ${T.options}`;
document.getElementById("title").textContent = T.title;
document.getElementById("enabledLabel").textContent = T.enabled;
document.getElementById("enabledHint").textContent = T.enabledHint;
document.getElementById("exclusionsLabel").textContent = T.exclusions;
document.getElementById("exclusionsHint").textContent = T.exclusionsHint;
document.getElementById("save").textContent = T.save;
document.getElementById("saved").textContent = T.saved;
document.getElementById("registrationLabel").textContent = T.registration;
document.getElementById("registrationHint").textContent = T.registrationHint;

// The registration command names THIS extension's id - the exact value
// the host manifest will pin.
const browser = isFirefox ? "firefox" : "chrome";
document.getElementById("registrationCmd").textContent =
  `udm-host --register ${browser} ${api.runtime.id}`;

const enabledBox = document.getElementById("enabled");
const exclusionsBox = document.getElementById("exclusions");

async function load() {
  const got = await api.storage.local.get(SETTINGS_KEY);
  const s = got[SETTINGS_KEY] || {};
  enabledBox.checked = s.enabled !== false;
  exclusionsBox.value = (Array.isArray(s.exclusions) ? s.exclusions : []).join("\n");
}

document.getElementById("save").addEventListener("click", async () => {
  const exclusions = exclusionsBox.value
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
  await api.storage.local.set({
    [SETTINGS_KEY]: { enabled: enabledBox.checked, exclusions },
  });
  const saved = document.getElementById("saved");
  saved.style.opacity = "1";
  setTimeout(() => (saved.style.opacity = "0"), 1500);
});

load();
