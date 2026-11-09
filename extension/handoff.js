"use strict";
/*
 * UDM Handoff - shared logic for the browser extension (G4).
 *
 * Implements the extension side of the frozen browser-handoff contract
 * (review-artifacts/zed-ipc-handoff-freeze-2026-11-08) over the wire
 * settled in the G5 design handoff: JSON objects {v: 1, op, tx, ...}
 * exchanged with the udm-host native messaging host, which bridges to
 * the coordinator in the UDM app.
 *
 * This file is environment-free: it touches no browser APIs, so the unit
 * tests can load it under Node. background.js wires it to chrome.* APIs.
 *
 * Contract points honoured here:
 *  - the transaction id is an opaque string, stable per download;
 *  - prepare must be ACKed before the browser download is cancelled;
 *  - commit is sent only after the browser-side cancel is confirmed;
 *  - if the coordinator is unavailable before prepare, or the cancel
 *    cannot be confirmed, the BROWSER keeps the download (we resume it
 *    and, for a prepared transaction, abort with browser_retains);
 *  - an expired commit answers recovery_required: the handoff is NOT
 *    retried automatically and the download is NOT restarted - the
 *    coordinator keeps the intent recoverable and the popup says so.
 */

const WIRE_VERSION = 1;

/** Host names registered by `udm-host --register` (one per browser). */
const HOST_NAMES = {
  chrome: "pw.cino.udm.handoff.chrome",
  firefox: "pw.cino.udm.handoff.firefox",
};

/** A UUID transaction id (opaque to everyone but us). */
function newTransactionId() {
  if (globalThis.crypto && crypto.randomUUID) return crypto.randomUUID();
  // Fallback for older Firefox: 122 random bits, formatted as a UUID.
  const b = new Uint8Array(16);
  (globalThis.crypto || {}).getRandomValues
    ? crypto.getRandomValues(b)
    : b.forEach((_, i) => (b[i] = Math.floor(Math.random() * 256)));
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

/**
 * Decide whether a browser download is eligible for handoff.
 * `item` is a chrome.downloads.DownloadItem-shaped object; `settings`
 * is {enabled, exclusions: [string]}. An exclusion matches when it is a
 * substring of the URL, or matches the URL's host exactly, or is a
 * "*.suffix" host glob, or a ".ext" file extension.
 */
function isEligible(item, settings) {
  if (!settings || settings.enabled === false) return false;
  if (!item || typeof item.url !== "string") return false;
  let url;
  try {
    url = new URL(item.url);
  } catch {
    return false;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return false;
  // A download the browser already finished or failed is not ours to move.
  if (item.state && item.state !== "in_progress") return false;
  const exclusions = Array.isArray(settings.exclusions) ? settings.exclusions : [];
  const filename = (item.filename || "").toLowerCase();
  for (const raw of exclusions) {
    const rule = String(raw).trim().toLowerCase();
    if (!rule) continue;
    if (rule.startsWith("*.")) {
      if (url.hostname === rule.slice(2) || url.hostname.endsWith(rule.slice(1))) return false;
    } else if (rule.startsWith(".")) {
      if (filename.endsWith(rule)) return false;
    } else if (url.hostname === rule) {
      return false;
    } else if (item.url.toLowerCase().includes(rule)) {
      return false;
    }
  }
  return true;
}

/**
 * Build the frozen intent snapshot from a download item. The intent is
 * material: the URL as the browser reports it (no normalization), the
 * browser's own download id as a decimal string, and display hints.
 */
function buildIntent(item) {
  const filename = item.filename || "";
  const base = filename.split(/[\\/]/).pop();
  return {
    url: item.finalUrl && item.finalUrl !== "" ? item.finalUrl : item.url,
    method: "GET",
    filename_hint: base ? base : null,
    size_hint: typeof item.fileSize === "number" && item.fileSize >= 0 ? item.fileSize : null,
    browser_download_id: String(item.id),
    dest_dir: null,
  };
}

/**
 * A serialized request/response client over a native messaging port.
 * One request is outstanding at a time (the design's transport bound);
 * replies are matched to requests in send order. `portFactory` returns a
 * fresh port ({postMessage, onMessage.addListener, onDisconnect.addListener,
 * disconnect}) or null when the host cannot be started.
 */
function createHandoffClient(portFactory) {
  let port = null;
  let pending = null; // {resolve} of the one outstanding request
  const queue = [];

  function ensurePort() {
    if (port) return port;
    const p = portFactory();
    if (!p) return null;
    port = p;
    port.onMessage.addListener((msg) => {
      if (pending) {
        const { resolve } = pending;
        pending = null;
        resolve(msg);
        pump();
      }
    });
    port.onDisconnect.addListener(() => {
      port = null;
      if (pending) {
        const { resolve } = pending;
        pending = null;
        resolve({ status: "unavailable" });
      }
      // Queued requests try a fresh port on their turn via pump().
      pump();
    });
    return port;
  }

  function pump() {
    if (pending || queue.length === 0) return;
    const next = queue.shift();
    const p = ensurePort();
    if (!p) {
      next.resolve({ status: "unavailable" });
      pump();
      return;
    }
    pending = next;
    try {
      p.postMessage(next.message);
    } catch {
      pending = null;
      port = null;
      next.resolve({ status: "unavailable" });
      pump();
    }
  }

  /** Send one wire message; resolves with the reply object. A transport
   *  failure resolves (never rejects) with {status: "unavailable"} - the
   *  freeze treats "coordinator unavailable" as an outcome, not an error. */
  function request(message) {
    return new Promise((resolve) => {
      queue.push({ message, resolve });
      pump();
    });
  }

  return {
    prepare(tx, intent) {
      return request({ v: WIRE_VERSION, op: "prepare", tx, intent });
    },
    commit(tx) {
      return request({
        v: WIRE_VERSION,
        op: "commit",
        tx,
        confirm: { browser_cancelled: true },
      });
    },
    abort(tx) {
      return request({
        v: WIRE_VERSION,
        op: "abort",
        tx,
        confirm: { browser_retains: true },
      });
    },
    query(tx) {
      return request({ v: WIRE_VERSION, op: "query", tx });
    },
  };
}

/**
 * The per-download handoff state machine, with every browser effect
 * injected through `fx` so tests can drive it:
 *   fx.pause(id), fx.resume(id), fx.cancel(id) -> Promise<bool>
 *     (cancel resolves true only when the browser confirms the download
 *      is cancelled; false when it completed or the cancel failed)
 *   fx.erase(id) -> Promise<void>
 *   fx.record(entry) -> Promise<void>  (bounded handoff log for the popup)
 * Returns the terminal outcome string.
 */
function createHandoffFlow(client, fx) {
  async function handoff(item) {
    const tx = newTransactionId();
    const intent = buildIntent(item);
    let prepared = false;
    try {
      // Stop bytes flowing while the coordinator decides; a pause failure
      // (already complete) makes us ineligible after the fact.
      await fx.pause(item.id).catch(() => false);

      const prep = await client.prepare(tx, intent);
      if (!prep || prep.status !== "prepared") {
        // Coordinator unavailable or conflicted before any promise: the
        // browser keeps the download, untouched.
        await fx.resume(item.id).catch(() => {});
        await fx.record({ tx, name: intent.filename_hint || intent.url, status: "kept-by-browser", at: Date.now() });
        return "kept-by-browser";
      }
      prepared = true;

      const cancelled = await fx.cancel(item.id).catch(() => false);
      if (!cancelled) {
        // The cancel was not confirmed (the download completed or the
        // browser refused): never commit on an unconfirmed cancel.
        await fx.abort(tx).catch(() => null);
        await fx.record({ tx, name: intent.filename_hint || intent.url, status: "kept-by-browser", at: Date.now() });
        return "kept-by-browser";
      }
      await fx.erase(item.id).catch(() => {});

      const commit = await client.commit(tx);
      if (commit && commit.status === "committed") {
        await fx.record({
          tx,
          name: intent.filename_hint || intent.url,
          status: "handed-off",
          task_ref: commit.task_ref || null,
          at: Date.now(),
        });
        return "handed-off";
      }
      if (commit && commit.status === "recovery-required") {
        // The lease expired between prepare and commit. The browser copy
        // is already cancelled; the coordinator keeps the intent
        // recoverable. Surface it - never silently re-download.
        await fx.record({ tx, name: intent.filename_hint || intent.url, status: "recovery-required", at: Date.now() });
        return "recovery-required";
      }
      await fx.record({ tx, name: intent.filename_hint || intent.url, status: "kept-by-browser", at: Date.now() });
      return "kept-by-browser";
    } catch {
      if (prepared) {
        // Best-effort release of the prepared intent.
        await client.abort(tx).catch(() => null);
      }
      await fx.resume(item.id).catch(() => {});
      return "kept-by-browser";
    }
  }
  return { handoff };
}

const api = {
  WIRE_VERSION,
  HOST_NAMES,
  newTransactionId,
  isEligible,
  buildIntent,
  createHandoffClient,
  createHandoffFlow,
};
if (typeof module !== "undefined" && module.exports) module.exports = api;
if (typeof globalThis !== "undefined") globalThis.UdmHandoff = api;
