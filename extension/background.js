"use strict";
/*
 * UDM Handoff - background entry point.
 *
 * Chrome MV3 loads this as the service worker (handoff.js is pulled in
 * via importScripts); Firefox MV3 loads handoff.js then this file as
 * background scripts. All contract logic lives in handoff.js; this file
 * only wires browser APIs to it.
 */

if (typeof importScripts === "function") {
  try {
    importScripts("handoff.js");
  } catch {
    /* Firefox script-list loading already provided UdmHandoff. */
  }
}

const H = globalThis.UdmHandoff;
const api = globalThis.browser || globalThis.chrome;
const HOST_NAME = globalThis.browser ? H.HOST_NAMES.firefox : H.HOST_NAMES.chrome;

const SETTINGS_KEY = "udmSettings";
const LOG_KEY = "udmHandoffLog";
const LOG_CAP = 20;

async function getSettings() {
  const got = await api.storage.local.get(SETTINGS_KEY);
  const s = got[SETTINGS_KEY] || {};
  return {
    enabled: s.enabled !== false, // auto-capture when the extension is enabled (product contract)
    exclusions: Array.isArray(s.exclusions) ? s.exclusions : [],
  };
}

async function record(entry) {
  const got = await api.storage.local.get(LOG_KEY);
  const log = Array.isArray(got[LOG_KEY]) ? got[LOG_KEY] : [];
  log.unshift(entry);
  await api.storage.local.set({ [LOG_KEY]: log.slice(0, LOG_CAP) });
}

/** Call a chrome.* API that may be callback- or promise-style. */
function invoke(fn, thisArg, args) {
  return new Promise((resolve, reject) => {
    try {
      const r = fn.apply(thisArg, [
        ...args,
        (value) => {
          const err = api.runtime.lastError;
          if (err) reject(new Error(err.message || String(err)));
          else resolve(value);
        },
      ]);
      if (r && typeof r.then === "function") r.then(resolve, reject);
    } catch (e) {
      reject(e);
    }
  });
}

const fx = {
  pause(id) {
    return invoke(api.downloads.pause, api.downloads, [id]).then(
      () => true,
      () => false,
    );
  },
  resume(id) {
    return invoke(api.downloads.resume, api.downloads, [id]).then(
      () => true,
      () => false,
    );
  },
  async cancel(id) {
    // A resolved cancel request is not yet a confirmation: verify the
    // resulting state. "interrupted" (or the item being gone) confirms;
    // "complete" means the browser copy finished and owns the file.
    try {
      await invoke(api.downloads.cancel, api.downloads, [id]);
    } catch {
      return false;
    }
    try {
      const items = await invoke(api.downloads.search, api.downloads, [{ id }]);
      if (!items || items.length === 0) return true;
      return items[0].state === "interrupted";
    } catch {
      return false;
    }
  },
  erase(id) {
    return invoke(api.downloads.erase, api.downloads, [{ id }]).then(
      () => {},
      () => {},
    );
  },
  record,
};

const client = H.createHandoffClient(() => {
  try {
    return api.runtime.connectNative(HOST_NAME);
  } catch {
    return null;
  }
});
const flow = H.createHandoffFlow(client, fx);

const inFlight = new Set();
api.downloads.onCreated.addListener((item) => {
  if (inFlight.has(item.id)) return;
  inFlight.add(item.id);
  (async () => {
    try {
      const settings = await getSettings();
      if (!H.isEligible(item, settings)) return;
      await flow.handoff(item);
    } catch {
      // A handoff failure must never disturb the browser download.
    } finally {
      inFlight.delete(item.id);
    }
  })();
});
