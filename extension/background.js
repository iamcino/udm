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
// Firefox is the only browser whose `browser` global implements
// runtime.getBrowserInfo; current Chrome also exposes a `browser`
// global, so its mere presence does not identify Firefox. Picking the
// wrong host name here makes every handoff fail with "host not found".
const IS_FIREFOX =
  !!globalThis.browser &&
  !!globalThis.browser.runtime &&
  typeof globalThis.browser.runtime.getBrowserInfo === "function";
const api = IS_FIREFOX ? globalThis.browser : globalThis.chrome || globalThis.browser;
const HOST_NAME = IS_FIREFOX ? H.HOST_NAMES.firefox : H.HOST_NAMES.chrome;

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

// Downloads this layer re-issued after a cancelled handoff attempt
// (see handoff.js): their onCreated must not start a second handoff.
// Keyed by URL - the new download's id is not known before its
// onCreated can fire - consumed on first sight and expired after two
// minutes. A wrongly skipped item is harmless: skipping only means
// the browser keeps that download, which is the fallback outcome.
const reissuedAt = new Map();
const REISSUE_WINDOW_MS = 120000;

const fx = {
  async cancel(id) {
    // A resolved cancel request is not yet a confirmation: verify the
    // resulting state. "interrupted" (or the item being gone) confirms;
    // "complete" means the browser copy finished and owns the file.
    // Chrome applies a cancel asynchronously - the state can still read
    // "in_progress" for a few hundred ms after the request resolves
    // (field case 2026-11-16: a single immediate check raced that
    // transition and Chrome handoffs aborted as unconfirmed), so poll
    // for the transition before giving up.
    try {
      await invoke(api.downloads.cancel, api.downloads, [id]);
    } catch {
      return false;
    }
    for (let waited = 0; ; waited += 100) {
      try {
        const items = await invoke(api.downloads.search, api.downloads, [{ id }]);
        if (!items || items.length === 0) return true;
        if (items[0].state === "interrupted") return true;
        if (items[0].state === "complete") return false;
      } catch {
        return false;
      }
      if (waited >= 2000) return false;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  },
  erase(id) {
    return invoke(api.downloads.erase, api.downloads, [{ id }]).then(
      () => {},
      () => {},
    );
  },
  download(url) {
    const now = Date.now();
    for (const [u, at] of reissuedAt) {
      if (now - at >= REISSUE_WINDOW_MS) reissuedAt.delete(u);
    }
    reissuedAt.set(url, now);
    return invoke(api.downloads.download, api.downloads, [{ url }]).then(
      (id) => id,
      (e) => {
        reissuedAt.delete(url);
        throw e;
      },
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
  const reissued = reissuedAt.get(item.url);
  if (reissued !== undefined) {
    reissuedAt.delete(item.url);
    if (Date.now() - reissued < REISSUE_WINDOW_MS) return;
  }
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
