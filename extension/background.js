"use strict";
/*
 * Universal Download Manager - background entry point.
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
const PENDING_KEY = "udmHandoffPending";
const LOG_CAP = 20;

async function getSettings() {
  const got = await api.storage.local.get(SETTINGS_KEY);
  const s = got[SETTINGS_KEY] || {};
  return {
    enabled: s.enabled !== false, // auto-capture when the extension is enabled (product contract)
    exclusions: Array.isArray(s.exclusions) ? s.exclusions : [],
  };
}

// Log writes share one chain so a migration and a new attention row cannot
// clobber each other. This chain is not the pending-journal chain: the
// pending map's semantics stay on `storageChain`.
let logChain = Promise.resolve();
function mutateLog(mutator) {
  const run = logChain.then(async () => {
    const got = await api.storage.local.get(LOG_KEY);
    const stored = got && Object.prototype.hasOwnProperty.call(got, LOG_KEY) ? got[LOG_KEY] : undefined;
    await mutator(stored);
  });
  logChain = run.then(
    () => {},
    () => {},
  );
  return run;
}

// Drop handed-off rows already in storage. The key stays, including when
// the cleaned value is an empty array. A missing or non-array value is
// left alone.
function migrateStoredLog() {
  return mutateLog(async (stored) => {
    const migrated = H.migrateHandoffLog(stored);
    if (!migrated || !Array.isArray(stored) || migrated.length === stored.length) return;
    await api.storage.local.set({ [LOG_KEY]: migrated });
  });
}

async function record(entry) {
  return mutateLog(async (stored) => {
    const migrated = H.migrateHandoffLog(stored);
    const hadArray = Array.isArray(stored);
    // Success presentation is not stored. An existing array that still
    // contains those rows is cleaned in place; the key is not removed.
    if (!entry || H.isSuccessLogStatus(entry.status)) {
      if (hadArray && migrated && migrated.length !== stored.length) {
        await api.storage.local.set({ [LOG_KEY]: migrated });
      }
      return;
    }
    const base = migrated ? migrated.slice() : [];
    base.unshift(entry);
    await api.storage.local.set({ [LOG_KEY]: base.slice(0, LOG_CAP) });
  });
}

// Pending handoffs, keyed by the extension's own transaction id. One
// chain so two downloads cannot clobber each other's record.
let storageChain = Promise.resolve();
function mutatePending(mutator) {
  const run = storageChain.then(async () => {
    const got = await api.storage.local.get(PENDING_KEY);
    const current = got && got[PENDING_KEY];
    const all =
      current && typeof current === "object" && !Array.isArray(current) ? Object.assign({}, current) : {};
    mutator(all);
    await api.storage.local.set({ [PENDING_KEY]: all });
  });
  storageChain = run.then(
    () => {},
    () => {},
  );
  return run;
}

function persistPending(rec) {
  return mutatePending((all) => {
    all[rec.tx] = rec;
  });
}

function clearPending(tx) {
  return mutatePending((all) => {
    delete all[tx];
  });
}

async function loadPending() {
  const got = await api.storage.local.get(PENDING_KEY);
  const all = got && got[PENDING_KEY];
  if (!all || typeof all !== "object" || Array.isArray(all)) return [];
  return Object.values(all).filter((rec) => rec && typeof rec.tx === "string" && rec.intent);
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
// Re-issue itself runs only for a download with no pending record, or
// for a pending record still at stage cancelled whose prepare answered
// unavailable. A prepared (or later) record is reconciled by query.
const reissuedAt = new Map();
const REISSUE_WINDOW_MS = 120000;

// Transactions whose flow is in this worker right now. A service-worker
// restart begins empty, which is what makes startup reload the journal.
const inFlightTx = new Set();
let resumeTimer = null;

const fx = {
  async cancel(id) {
    // A resolved cancel request is not yet a confirmation: verify the
    // resulting state. "interrupted" (or the item being gone) confirms;
    // "complete" means the browser copy finished and owns the file.
    // Chrome applies a cancel asynchronously - the state can still read
    // "in_progress" for a few hundred ms after the request resolves
    // (field case 2026-11-16: a single immediate check raced that
    // transition and Chrome handoffs aborted as unconfirmed), so poll
    // for the transition before giving up. A restart can also observe
    // an item that was already erased: the cancel request throws, and
    // an empty search is still confirmation, not a reason to skip prepare.
    let requested = false;
    try {
      await invoke(api.downloads.cancel, api.downloads, [id]);
      requested = true;
    } catch {
      requested = false;
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
      if (!requested || waited >= 2000) return false;
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
  persist: persistPending,
  clearPending,
  claimTx(tx) {
    if (inFlightTx.has(tx)) return false;
    inFlightTx.add(tx);
    return true;
  },
  releaseTx(tx) {
    inFlightTx.delete(tx);
  },
  notifyPending() {
    scheduleResume();
  },
};

function scheduleResume() {
  if (resumeTimer !== null) return;
  // The dropped port (or a lost reply) is not itself an outcome. Ask
  // again once the next connection can be opened.
  resumeTimer = setTimeout(() => {
    resumeTimer = null;
    resumePending();
  }, H.POLL_MS);
}

function resumePending() {
  loadPending()
    .then((records) => {
      for (const rec of records) flow.resume(rec);
    })
    .catch(() => {});
}

const client = H.createHandoffClient(
  () => {
    try {
      return api.runtime.connectNative(HOST_NAME);
    } catch {
      return null;
    }
  },
  { onDisconnect: scheduleResume },
);
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
      // Same browser download already has a transaction. Resume that
      // record instead of allocating a second one.
      const existing = (await loadPending()).find(
        (rec) => rec.intent && rec.intent.browser_download_id === String(item.id),
      );
      if (existing) {
        await flow.resume(existing);
        return;
      }
      await flow.handoff(item);
    } catch {
      // A handoff failure must never disturb the browser download.
    } finally {
      inFlight.delete(item.id);
    }
  })();
});

// Service-worker startup: strip handed-off history from the shared log,
// then reconcile whatever was durable when the worker last stopped.
// Prepared (and later) records are queried; they are not cancelled and
// not re-issued. Migration does not read or write the pending journal.
migrateStoredLog().then(
  () => resumePending(),
  () => resumePending(),
);
