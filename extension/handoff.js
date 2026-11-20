"use strict";
/*
 * UDM Handoff - shared logic for the browser extension (G4).
 *
 * Implements the extension side of the browser-handoff contract over
 * the wire settled in the G5 design handoff: JSON objects {v: 1, op,
 * tx, ...} exchanged with the udm-host native messaging host, which
 * bridges to the coordinator in the UDM app.
 *
 * This file is environment-free: it touches no browser APIs, so the unit
 * tests can load it under Node. background.js wires it to chrome.* APIs.
 *
 * Contract points honoured here:
 *  - the transaction id is an opaque string, stable per download, and a
 *    pending record is never given a replacement id;
 *  - before any browser effect, the pending record (tx + intent +
 *    stage) is durable. The stage advances recorded → cancelled →
 *    prepared → a terminal outcome, and that outcome is durable before
 *    the pending record is cleared;
 *  - the browser download is cancelled FIRST (0.2.3): with "ask where
 *    to save" on, Chrome parks a new download behind its Save As
 *    prompt and no other browser effect lands until the user dismisses
 *    it - the old first step (pause) stalled there, and the handoff
 *    only completed after the user cancelled the prompt by hand
 *    (field case 2026-11-17). Cancelling a prompt-parked download
 *    dismisses the prompt. The freeze's no-loss property is kept by
 *    the re-issue fallback below instead of by the old ordering;
 *  - prepare carries browser_cancelled: true only after that cancel
 *    and the following erase are confirmed for this transaction. The
 *    GUI commits or discards; this side learns the result by polling
 *    query (about once a second, slower while association is pending);
 *  - a lost reply is an unknown outcome: query the same tx and act
 *    only on that answer. A record at prepared or later is never
 *    re-issued and its cancel/erase is never repeated;
 *  - if prepare answers unavailable while the record is still
 *    cancelled (the GUI is not running), the download is RE-ISSUED in
 *    the browser — the pre-existing lossy fallback. The wiring layer
 *    (background.js) marks re-issued downloads so they are not handed
 *    off a second time;
 *  - if the cancel itself cannot be confirmed, the browser copy
 *    stands untouched and prepare is never sent;
 *  - recovery-required means the commitment is durable but the task
 *    association is still pending. Keep the record, poll about every
 *    5 s, surface it on the existing handoff log, and never re-issue.
 *    query of an expired record (or recovery-required whose state is
 *    expired) is the expired outcome.
 */

const WIRE_VERSION = 1;
const POLL_MS = 1000;
const RECOVERY_POLL_MS = 5000;

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
 * `hooks.onDisconnect` fires when the live port drops so the wiring
 * layer can resume pending records on the next connection.
 */
function createHandoffClient(portFactory, hooks) {
  let port = null;
  let pending = null; // {resolve} of the one outstanding request
  const queue = [];
  const onDisconnect = hooks && typeof hooks.onDisconnect === "function" ? hooks.onDisconnect : null;

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
      if (onDisconnect) onDisconnect();
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
    prepare(tx, intent, attestation) {
      const message = { v: WIRE_VERSION, op: "prepare", tx, intent };
      // Top-level boolean: the prepare-time attestation the coordinator
      // stores as CancelledForHandoff. Omitted unless this transaction's
      // cancel and erase have both been confirmed.
      if (attestation && attestation.browser_cancelled === true) {
        message.browser_cancelled = true;
      }
      return request(message);
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

function intentSnapshot(intent) {
  return {
    url: intent.url,
    method: intent.method || "GET",
    filename_hint: intent.filename_hint ?? null,
    size_hint: intent.size_hint ?? null,
    browser_download_id: String(intent.browser_download_id),
    dest_dir: intent.dest_dir ?? null,
  };
}

function pendingRecord(tx, intent, stage, extra) {
  const rec = {
    tx,
    intent: intentSnapshot(intent),
    stage,
    task_ref: null,
    reissued: false,
  };
  if (extra) {
    if (extra.task_ref !== undefined) rec.task_ref = extra.task_ref;
    if (extra.reissued) rec.reissued = true;
    if (extra.reissueFailed) rec.reissueFailed = true;
  }
  return rec;
}

function browserDownloadId(intent) {
  const raw = intent.browser_download_id;
  if (typeof raw === "number" && Number.isFinite(raw)) return raw;
  if (typeof raw === "string" && /^[0-9]+$/.test(raw)) return Number(raw);
  return raw;
}

function taskRefOf(reply) {
  if (!reply || reply.task_ref == null || reply.task_ref === "") return null;
  return String(reply.task_ref);
}

/**
 * The per-download handoff state machine, with every browser and storage
 * effect injected through `fx` so tests can drive it:
 *   fx.cancel(id) -> Promise<bool>
 *     (resolves true only when the browser confirms the download is
 *      cancelled; false when it completed or the cancel failed)
 *   fx.erase(id) -> Promise<void>
 *   fx.download(url) -> Promise<id>  (re-issue a download in the
 *     browser; rejects when the browser refuses)
 *   fx.record(entry) -> Promise<void>  (bounded handoff log for the popup)
 *   fx.persist(record) -> Promise<void>  (durable pending record)
 *   fx.clearPending(tx) -> Promise<void>
 *   fx.sleep(ms) -> Promise<void>  (optional; defaults to setTimeout)
 *   fx.claimTx(tx) / fx.releaseTx(tx) / fx.notifyPending() optional
 * `options.pollMs` (default 1000) and `options.recoveryPollMs` (default
 * 5000) are the waiting-loop intervals.
 * Returns the terminal outcome string, or "pending" when the port dropped
 * and the same record must be resumed later.
 */
function createHandoffFlow(client, fx, options) {
  const opts = options || {};
  const pollMs = opts.pollMs ?? POLL_MS;
  const recoveryPollMs = opts.recoveryPollMs ?? RECOVERY_POLL_MS;
  const sleep =
    typeof fx.sleep === "function"
      ? (ms) => fx.sleep(ms)
      : (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  async function persist(rec) {
    await fx.persist(rec);
  }
  async function clear(tx) {
    await fx.clearPending(tx);
  }
  function notify() {
    if (typeof fx.notifyPending === "function") fx.notifyPending();
  }

  async function settle(tx, intent, stage, taskRef) {
    const popup = stage === "committed" ? "handed-off" : stage;
    // The outcome hits storage before the pending record is removed, so a
    // crash in between resumes the same tx instead of inventing one.
    await persist(pendingRecord(tx, intent, stage, { task_ref: taskRef || null }));
    await fx.record({
      tx,
      name: intent.filename_hint || intent.url,
      status: popup,
      task_ref: taskRef || null,
      at: Date.now(),
    });
    await clear(tx);
    return popup;
  }

  async function wait(tx, intent, stage) {
    let current = stage;
    for (;;) {
      const reply = await client.query(tx);
      if (!reply || reply.status === "unavailable") {
        notify();
        return "pending";
      }
      if (reply.status === "committed") return settle(tx, intent, "committed", taskRefOf(reply));
      if (reply.status === "aborted" || reply.status === "discarded" || reply.status === "expired") {
        return settle(tx, intent, reply.status, null);
      }
      if (reply.status === "recovery-required") {
        // A late view of an expired record is expiry, not a pending association.
        if (reply.state === "expired") return settle(tx, intent, "expired", null);
        if (current !== "recovery-required") {
          current = "recovery-required";
          await persist(pendingRecord(tx, intent, "recovery-required"));
          await fx.record({
            tx,
            name: intent.filename_hint || intent.url,
            status: "recovery-required",
            at: Date.now(),
          });
        }
        await sleep(recoveryPollMs);
        continue;
      }
      // prepared, not-found, conflict: not a licence to re-issue or to
      // allocate another transaction. Keep asking this one.
      await sleep(current === "recovery-required" ? recoveryPollMs : pollMs);
    }
  }

  async function enterRecovery(tx, intent) {
    await persist(pendingRecord(tx, intent, "recovery-required"));
    await fx.record({
      tx,
      name: intent.filename_hint || intent.url,
      status: "recovery-required",
      at: Date.now(),
    });
    return wait(tx, intent, "recovery-required");
  }

  /** Act on a query (or a prepare replay) that names a real record.
   *  Returns null when the reply is missing, unavailable, or not-found. */
  async function reconcileQuery(tx, intent, reply) {
    if (!reply || reply.status === "unavailable" || reply.status === "not-found") return null;
    if (reply.status === "prepared") {
      await persist(pendingRecord(tx, intent, "prepared"));
      return wait(tx, intent, "prepared");
    }
    if (reply.status === "recovery-required") {
      if (reply.state === "expired") return settle(tx, intent, "expired", null);
      return enterRecovery(tx, intent);
    }
    if (reply.status === "committed") return settle(tx, intent, "committed", taskRefOf(reply));
    if (reply.status === "aborted" || reply.status === "discarded" || reply.status === "expired") {
      return settle(tx, intent, reply.status, null);
    }
    await persist(pendingRecord(tx, intent, "conflict"));
    await fx.record({
      tx,
      name: intent.url,
      status: "recovery-required",
      at: Date.now(),
    });
    await clear(tx);
    return "recovery-required";
  }

  // Give the download back to the browser as a fresh download after the
  // original was cancelled and prepare could not reach a running GUI.
  // Attempted at most once per record: the reissued flag is durable
  // before fx.download, so a restart cannot issue a second copy.
  async function finishReissue(tx, intent) {
    let ok = false;
    await persist(pendingRecord(tx, intent, "cancelled", { reissued: true }));
    try {
      await fx.download(intent.url);
      ok = true;
    } catch {
      ok = false;
    }
    if (ok) {
      await persist(pendingRecord(tx, intent, "kept-by-browser", { reissued: true }));
      await fx.record({
        tx,
        name: intent.filename_hint || intent.url,
        status: "kept-by-browser",
        at: Date.now(),
      });
    } else {
      await persist(pendingRecord(tx, intent, "kept-by-browser", { reissued: true, reissueFailed: true }));
      await fx.record({
        tx,
        name: intent.url,
        status: "recovery-required",
        at: Date.now(),
      });
    }
    await clear(tx);
    return "kept-by-browser";
  }

  async function handlePrepare(tx, intent, prep, allowRetry) {
    if (prep && prep.status === "prepared") {
      await persist(pendingRecord(tx, intent, "prepared"));
      return wait(tx, intent, "prepared");
    }
    if (prep && prep.status === "committed") return settle(tx, intent, "committed", taskRefOf(prep));
    if (prep && (prep.status === "aborted" || prep.status === "discarded" || prep.status === "expired")) {
      return settle(tx, intent, prep.status, null);
    }
    if (prep && prep.status === "recovery-required") {
      if (prep.state === "expired") return settle(tx, intent, "expired", null);
      return enterRecovery(tx, intent);
    }
    if (!prep || prep.status === "unavailable") {
      // The prepare reply was lost, or the host never came up. Query
      // before deciding; re-issue only when that query also cannot show
      // a live or terminal record.
      const q = await client.query(tx);
      const acted = await reconcileQuery(tx, intent, q);
      if (acted !== null) return acted;
      if (allowRetry && q && q.status === "not-found") {
        const again = await client.prepare(tx, intent, { browser_cancelled: true });
        return handlePrepare(tx, intent, again, false);
      }
      return finishReissue(tx, intent);
    }
    const q = await client.query(tx);
    if (q && q.status && q.status !== "unavailable" && q.status !== "not-found" && q.status !== "conflict") {
      const acted = await reconcileQuery(tx, intent, q);
      if (acted !== null) return acted;
    }
    await persist(pendingRecord(tx, intent, "conflict"));
    await fx.record({
      tx,
      name: intent.url,
      status: "recovery-required",
      at: Date.now(),
    });
    await clear(tx);
    return "recovery-required";
  }

  async function reconcileSettled(tx, intent, stage, prior) {
    if (stage === "committed" || stage === "aborted" || stage === "discarded" || stage === "expired") {
      const q = await client.query(tx);
      if (!q || q.status === "unavailable") {
        notify();
        return "pending";
      }
      const acted = await reconcileQuery(tx, intent, q);
      if (acted !== null) return acted;
    }
    const failed = !!prior.reissueFailed || stage === "conflict";
    const popup =
      stage === "committed"
        ? "handed-off"
        : failed
          ? "recovery-required"
          : stage === "kept-by-browser"
            ? "kept-by-browser"
            : stage;
    await fx.record({
      tx,
      name: failed ? intent.url : intent.filename_hint || intent.url,
      status: popup,
      task_ref: prior.task_ref || null,
      at: Date.now(),
    });
    await clear(tx);
    return stage === "kept-by-browser" ? "kept-by-browser" : popup;
  }

  async function drive(tx, intent, stage, browserId, flags) {
    const prior = (flags && flags.pending) || {};
    const fresh = !!(flags && flags.fresh);
    const resumed = !!(flags && flags.resumed);

    if (fresh) {
      await persist(pendingRecord(tx, intent, "recorded"));
      stage = "recorded";
    } else if (
      stage !== "recorded" &&
      stage !== "cancelled" &&
      stage !== "prepared" &&
      stage !== "recovery-required"
    ) {
      return reconcileSettled(tx, intent, stage, prior);
    }

    if (stage === "recorded") {
      if (!fresh) {
        const early = await reconcileQuery(tx, intent, await client.query(tx));
        if (early !== null) return early;
      }
      // Cancel FIRST (0.2.3): a prompt-parked download accepts no other
      // effect until the Save As prompt is dismissed, and cancelling
      // dismisses it. An unconfirmed cancel means the browser copy
      // stands (it completed, or the browser refused): prepare is
      // never sent and nothing is lost.
      const cancelled = await fx.cancel(browserId).catch(() => false);
      if (!cancelled) {
        await persist(pendingRecord(tx, intent, "kept-by-browser"));
        await fx.record({
          tx,
          name: intent.filename_hint || intent.url,
          status: "kept-by-browser",
          at: Date.now(),
        });
        await clear(tx);
        return "kept-by-browser";
      }
      await fx.erase(browserId).catch(() => {});
      await persist(pendingRecord(tx, intent, "cancelled"));
      stage = "cancelled";
    }

    if (stage === "cancelled") {
      if (prior.reissued) {
        await fx.record({
          tx,
          name: prior.reissueFailed ? intent.url : intent.filename_hint || intent.url,
          status: prior.reissueFailed ? "recovery-required" : "kept-by-browser",
          at: Date.now(),
        });
        await clear(tx);
        return "kept-by-browser";
      }
      if (resumed) {
        const early = await reconcileQuery(tx, intent, await client.query(tx));
        if (early !== null) return early;
      }
      const prep = await client.prepare(tx, intent, { browser_cancelled: true });
      return handlePrepare(tx, intent, prep, true);
    }

    if (stage === "prepared" || stage === "recovery-required") {
      return wait(tx, intent, stage);
    }
    return "pending";
  }

  async function guarded(tx, run) {
    if (typeof fx.claimTx === "function" && fx.claimTx(tx) === false) return "pending";
    try {
      return await run();
    } catch {
      notify();
      return "pending";
    } finally {
      if (typeof fx.releaseTx === "function") fx.releaseTx(tx);
    }
  }

  async function handoff(item) {
    const tx = newTransactionId();
    const intent = buildIntent(item);
    return guarded(tx, () => drive(tx, intent, "recorded", item.id, { fresh: true }));
  }

  async function resume(pending) {
    if (!pending || typeof pending.tx !== "string" || !pending.intent) return "pending";
    // The stored id is the transaction. Never allocate another one, and
    // never cancel/erase again once the stage is cancelled or later —
    // drive() only cancels from "recorded".
    return guarded(pending.tx, () =>
      drive(pending.tx, pending.intent, pending.stage || "recorded", browserDownloadId(pending.intent), {
        resumed: true,
        pending,
      }),
    );
  }

  return { handoff, resume };
}

const handoffApi = {
  WIRE_VERSION,
  HOST_NAMES,
  POLL_MS,
  RECOVERY_POLL_MS,
  newTransactionId,
  isEligible,
  buildIntent,
  createHandoffClient,
  createHandoffFlow,
};
if (typeof module !== "undefined" && module.exports) module.exports = handoffApi;
if (typeof globalThis !== "undefined") globalThis.UdmHandoff = handoffApi;
