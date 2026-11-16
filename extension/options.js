"use strict";
// Firefox is the only browser whose `browser` global implements
// runtime.getBrowserInfo; current Chrome also exposes a `browser`
// global, so its mere presence does not identify Firefox.
const isFirefox =
  !!globalThis.browser &&
  !!globalThis.browser.runtime &&
  typeof globalThis.browser.runtime.getBrowserInfo === "function";
const api = isFirefox ? globalThis.browser : globalThis.chrome || globalThis.browser;
const T = globalThis.UdmStrings.udmT();
const SETTINGS_KEY = "udmSettings";

document.title = `${T.title} - ${T.options}`;
document.getElementById("title").textContent = T.title;
document.getElementById("enabledLabel").textContent = T.enabled;
document.getElementById("enabledHint").textContent = T.enabledHint;
document.getElementById("exclusionsLabel").textContent = T.exclusions;
document.getElementById("exclusionsHint").textContent = T.exclusionsHint;
document.getElementById("save").textContent = T.save;
document.getElementById("saved").textContent = T.saved;
document.getElementById("registrationLabel").textContent = T.registration;
document.getElementById("registrationHint").textContent =
  isFirefox ? T.registrationHintFirefox : T.registrationHintChrome;

// Registration happens in UDM Settings; this page only shows the ID.
// Chrome: the user pastes this ID into UDM. Firefox: the ID is fixed
// (udm-handoff@cino.pw), so UDM needs no value at all.
document.getElementById("registrationCmd").textContent = api.runtime.id;

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
