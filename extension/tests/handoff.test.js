"use strict";
/*
 * Unit tests for the UDM Handoff extension logic (extension/handoff.js).
 * Run:  node extension/tests/handoff.test.js
 * No dependencies beyond Node itself.
 */
const assert = require("node:assert");
const path = require("node:path");
const H = require(path.join(__dirname, "..", "handoff.js"));

let passed = 0;
async function test(name, fn) {
  await fn();
  passed += 1;
  console.log(`ok - ${name}`);
}

function item(overrides) {
  return Object.assign(
    {
      id: 42,
      url: "https://files.example.com/pub/report.pdf",
      finalUrl: "",
      filename: "C:\\Users\\x\\Downloads\\report.pdf",
      fileSize: 12345,
      state: "in_progress",
    },
    overrides,
  );
}

(async () => {
  // ---- eligibility ----------------------------------------------------
  await test("eligible by default for http(s)", () => {
    assert.strictEqual(H.isEligible(item(), { enabled: true, exclusions: [] }), true);
    assert.strictEqual(H.isEligible(item({ url: "http://a.b/c.bin" }), { enabled: true, exclusions: [] }), true);
  });

  await test("disabled or non-http(s) or finished downloads are not eligible", () => {
    assert.strictEqual(H.isEligible(item(), { enabled: false, exclusions: [] }), false);
    assert.strictEqual(H.isEligible(item({ url: "ftp://a.b/c" }), { enabled: true, exclusions: [] }), false);
    assert.strictEqual(H.isEligible(item({ url: "blob:https://a.b/x" }), { enabled: true, exclusions: [] }), false);
    assert.strictEqual(H.isEligible(item({ state: "complete" }), { enabled: true, exclusions: [] }), false);
  });

  await test("exclusions: host, host suffix, extension, substring", () => {
    const s = (exclusions) => ({ enabled: true, exclusions });
    assert.strictEqual(H.isEligible(item(), s(["files.example.com"])), false);
    assert.strictEqual(H.isEligible(item(), s(["*.example.com"])), false);
    assert.strictEqual(H.isEligible(item(), s([".pdf"])), false);
    assert.strictEqual(H.isEligible(item(), s(["report"])), false);
    assert.strictEqual(H.isEligible(item(), s(["other.example.com", ".iso"])), true);
    assert.strictEqual(H.isEligible(item({ url: "https://sub.example.com/x" }), s(["*.example.com"])), false);
    assert.strictEqual(H.isEligible(item({ url: "https://example.com/x" }), s(["*.example.com"])), false);
  });

  // ---- intent ---------------------------------------------------------
  await test("buildIntent carries the frozen fields", () => {
    const i = H.buildIntent(item());
    assert.strictEqual(i.url, "https://files.example.com/pub/report.pdf");
    assert.strictEqual(i.method, "GET");
    assert.strictEqual(i.filename_hint, "report.pdf");
    assert.strictEqual(i.size_hint, 12345);
    assert.strictEqual(i.browser_download_id, "42");
    assert.strictEqual(i.dest_dir, null);
    const j = H.buildIntent(item({ finalUrl: "https://cdn.example.com/final.pdf", fileSize: -1 }));
    assert.strictEqual(j.url, "https://cdn.example.com/final.pdf");
    assert.strictEqual(j.size_hint, null);
  });

  // ---- wire client ----------------------------------------------------
  function fakePort() {
    const listeners = { msg: [], disc: [] };
    return {
      sent: [],
      postMessage(m) {
        this.sent.push(m);
      },
      onMessage: { addListener: (f) => listeners.msg.push(f) },
      onDisconnect: { addListener: (f) => listeners.disc.push(f) },
      disconnect() {},
      _reply(m) {
        listeners.msg.forEach((f) => f(m));
      },
      _disconnect() {
        listeners.disc.forEach((f) => f());
      },
    };
  }

  await test("client serializes requests and matches replies in order", async () => {
    const port = fakePort();
    const client = H.createHandoffClient(() => port);
    const p1 = client.prepare("tx-1", { url: "https://a.b/f" });
    const p2 = client.query("tx-1");
    // Only the first request is on the wire so far.
    assert.strictEqual(port.sent.length, 1);
    assert.strictEqual(port.sent[0].op, "prepare");
    assert.strictEqual(port.sent[0].v, 1);
    port._reply({ v: 1, op: "prepare", tx: "tx-1", status: "prepared" });
    const r1 = await p1;
    assert.strictEqual(r1.status, "prepared");
    assert.strictEqual(port.sent.length, 2);
    assert.strictEqual(port.sent[1].op, "query");
    port._reply({ v: 1, op: "query", tx: "tx-1", status: "not-found" });
    assert.strictEqual((await p2).status, "not-found");
  });

  await test("client resolves unavailable when the host cannot start or drops", async () => {
    const none = H.createHandoffClient(() => null);
    assert.strictEqual((await none.query("tx")).status, "unavailable");
    const port = fakePort();
    const client = H.createHandoffClient(() => port);
    const p = client.commit("tx-9");
    port._disconnect();
    assert.strictEqual((await p).status, "unavailable");
  });

  await test("client puts browser_cancelled on prepare only when attested", async () => {
    const port = fakePort();
    const client = H.createHandoffClient(() => port);
    const attested = client.prepare("tx-1", { url: "https://a.b/f" }, { browser_cancelled: true });
    assert.strictEqual(port.sent[0].op, "prepare");
    assert.strictEqual(port.sent[0].browser_cancelled, true);
    port._reply({ status: "prepared" });
    await attested;
    const plain = client.prepare("tx-2", { url: "https://a.b/g" });
    assert.strictEqual(port.sent[1].browser_cancelled, undefined);
    port._reply({ status: "prepared" });
    await plain;
  });

  // ---- handoff flow ---------------------------------------------------
  function fakeFx(behavior) {
    const calls = [];
    const pending = new Map();
    return {
      calls,
      pending,
      cancel: async (id) => (calls.push(["cancel", id]), behavior.cancel),
      erase: async (id) => calls.push(["erase", id]),
      download: async (url) => {
        calls.push(["download", url]);
        if (behavior.downloadFails) throw new Error("browser refused");
        return 99;
      },
      record: async (e) => calls.push(["record", e.status, e.name, e.task_ref || null]),
      persist: async (rec) => {
        calls.push(["persist", rec.stage, rec.tx, rec.intent, rec.task_ref || null]);
        pending.set(rec.tx, JSON.parse(JSON.stringify(rec)));
      },
      clearPending: async (tx) => {
        calls.push(["clear", tx]);
        pending.delete(tx);
      },
      sleep: async (ms) => {
        calls.push(["sleep", ms, [...pending.values()].map((rec) => rec.stage)]);
      },
    };
  }
  function fakeClient(replies) {
    const calls = [];
    const sent = [];
    const take = (op) => {
      const spec = replies[op];
      if (typeof spec === "function") return spec();
      if (Array.isArray(spec)) return spec.length ? spec.shift() : { status: "unavailable" };
      return spec;
    };
    return {
      calls,
      sent,
      prepare: async (tx, intent, attestation) => {
        calls.push("prepare");
        sent.push({
          op: "prepare",
          tx,
          intent,
          browser_cancelled: !!(attestation && attestation.browser_cancelled),
        });
        return take("prepare");
      },
      commit: async (tx) => {
        calls.push("commit");
        sent.push({ op: "commit", tx });
        return take("commit");
      },
      abort: async (tx) => {
        calls.push("abort");
        sent.push({ op: "abort", tx });
        return take("abort");
      },
      query: async (tx) => {
        calls.push("query");
        sent.push({ op: "query", tx });
        return take("query");
      },
    };
  }

  function persistBefore(calls, stage) {
    return calls.find((c) => c[0] === "persist" && c[1] === stage);
  }

  await test("pending record is persisted before cancel is attempted", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "prepared" },
      query: { status: "committed", task_ref: "1001" },
    });
    await H.createHandoffFlow(client, fx).handoff(item());
    const recorded = persistBefore(fx.calls, "recorded");
    const cancelAt = fx.calls.findIndex((c) => c[0] === "cancel");
    assert.ok(recorded, "a recorded-stage pending record must exist");
    assert.ok(fx.calls.indexOf(recorded) < cancelAt);
    assert.strictEqual(recorded[3].url, "https://files.example.com/pub/report.pdf");
    assert.strictEqual(recorded[3].filename_hint, "report.pdf");
    assert.strictEqual(recorded[3].size_hint, 12345);
    assert.strictEqual(recorded[3].browser_download_id, "42");
    assert.strictEqual(recorded[3].dest_dir, null);
    assert.strictEqual(recorded[3].method, "GET");
    // The cancel is still the first browser effect (0.2.3).
    const browser = fx.calls.filter((c) => c[0] === "cancel" || c[0] === "erase" || c[0] === "download");
    assert.deepStrictEqual(browser[0], ["cancel", 42]);
  });

  await test("prepare carries browser_cancelled only after cancel and erase are confirmed", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "prepared" },
      query: { status: "committed", task_ref: "1001" },
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "handed-off");
    const prep = client.sent.find((s) => s.op === "prepare");
    assert.strictEqual(prep.browser_cancelled, true);
    assert.strictEqual(prep.intent.url, "https://files.example.com/pub/report.pdf");
    const cancelAt = fx.calls.findIndex((c) => c[0] === "cancel");
    const eraseAt = fx.calls.findIndex((c) => c[0] === "erase");
    const cancelledAt = fx.calls.findIndex((c) => c[0] === "persist" && c[1] === "cancelled");
    assert.ok(cancelAt !== -1 && eraseAt > cancelAt && cancelledAt > eraseAt);
    assert.ok(!client.calls.includes("commit"));
    assert.ok(!client.calls.includes("abort"));

    const refused = fakeFx({ cancel: false });
    const refusedClient = fakeClient({ prepare: { status: "prepared" } });
    const kept = await H.createHandoffFlow(refusedClient, refused).handoff(item());
    assert.strictEqual(kept, "kept-by-browser");
    assert.deepStrictEqual(refusedClient.calls, []);
    assert.ok(persistBefore(refused.calls, "recorded"));
    assert.ok(!refused.calls.some((c) => c[0] === "download"));
  });

  async function settleVia(status, reply, expected, taskRef) {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "prepared" },
      query: [{ status: "prepared" }, reply],
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, expected);
    assert.ok(fx.calls.some((c) => c[0] === "sleep" && c[1] === H.POLL_MS));
    const recorded = fx.calls.find((c) => c[0] === "record" && c[1] === expected);
    assert.ok(recorded, expected);
    assert.strictEqual(recorded[3], taskRef);
    const outcomeAt = fx.calls.findIndex((c) => c[0] === "persist" && c[1] === status);
    const recordAt = fx.calls.indexOf(recorded);
    const clearAt = fx.calls.findIndex((c) => c[0] === "clear");
    assert.ok(outcomeAt !== -1 && outcomeAt < recordAt && recordAt < clearAt);
    assert.strictEqual(fx.pending.size, 0);
    assert.ok(!fx.calls.some((c) => c[0] === "download"));
    assert.ok(!client.calls.includes("commit"));
    return { fx, client };
  }

  await test("waiting loop resolves on a committed reply and records task_ref", async () => {
    const { fx } = await settleVia("committed", { status: "committed", task_ref: "1001" }, "handed-off", "1001");
    assert.ok(fx.calls.some((c) => c[0] === "record" && c[1] === "handed-off" && c[2] === "report.pdf"));
  });

  await test("waiting loop resolves on a discarded reply", async () => {
    await settleVia("discarded", { status: "discarded" }, "discarded", null);
  });

  await test("waiting loop resolves on an expired reply", async () => {
    await settleVia("expired", { status: "expired" }, "expired", null);
  });

  await test("recovery-required keeps polling and never re-issues", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "prepared" },
      query: [
        { status: "recovery-required", state: "committed" },
        { status: "recovery-required", state: "committed" },
        { status: "committed", task_ref: "9" },
      ],
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "handed-off");
    const sleeps = fx.calls.filter((c) => c[0] === "sleep");
    assert.deepStrictEqual(
      sleeps.map((c) => c[1]),
      [H.RECOVERY_POLL_MS, H.RECOVERY_POLL_MS],
    );
    assert.ok(sleeps.every((c) => c[2].includes("recovery-required")));
    const recoveryRecords = fx.calls.filter((c) => c[0] === "record" && c[1] === "recovery-required");
    assert.strictEqual(recoveryRecords.length, 1);
    assert.ok(!fx.calls.some((c) => c[0] === "download"));
    assert.strictEqual(client.calls.filter((op) => op === "prepare").length, 1);
    assert.ok(!client.calls.includes("commit"));
    assert.strictEqual(fx.pending.size, 0);
  });

  await test("a lost prepare reply is queried before any re-issue", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "unavailable" },
      query: [{ status: "prepared" }, { status: "discarded" }],
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "discarded");
    const tx = persistBefore(fx.calls, "recorded")[2];
    assert.ok(client.sent.filter((s) => s.op === "query").every((s) => s.tx === tx));
    assert.ok(!fx.calls.some((c) => c[0] === "download"));
    assert.ok(!client.calls.includes("commit"));
  });

  await test("prepare conflict is not re-issued", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "conflict" },
      query: { status: "not-found" },
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "recovery-required");
    assert.ok(!fx.calls.some((c) => c[0] === "download"));
    assert.strictEqual(fx.pending.size, 0);
  });

  await test("flow: coordinator unavailable re-issues the download in the browser", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "unavailable" },
      query: { status: "unavailable" },
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "kept-by-browser");
    assert.ok(client.calls.includes("prepare"));
    assert.ok(client.calls.includes("query"));
    assert.deepStrictEqual(
      fx.calls.find((c) => c[0] === "cancel"),
      ["cancel", 42],
    );
    assert.ok(fx.calls.some((c) => c[0] === "download" && c[1] === "https://files.example.com/pub/report.pdf"));
    assert.ok(fx.calls.some((c) => c[0] === "record" && c[1] === "kept-by-browser"));
    assert.strictEqual(fx.calls.filter((c) => c[0] === "download").length, 1);
    assert.strictEqual(fx.pending.size, 0);
  });

  await test("flow: unconfirmed cancel never prepares and loses nothing", async () => {
    const fx = fakeFx({ cancel: false });
    const client = fakeClient({ prepare: { status: "prepared" }, query: { status: "prepared" } });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "kept-by-browser");
    // The browser copy stands (it completed or refused the cancel):
    // prepare is never sent and no re-issue happens.
    assert.deepStrictEqual(client.calls, []);
    assert.ok(fx.calls.indexOf(persistBefore(fx.calls, "recorded")) < fx.calls.findIndex((c) => c[0] === "cancel"));
    assert.ok(!fx.calls.some((c) => c[0] === "download"));
    assert.ok(
      fx.calls.some((c) => c[0] === "record" && c[1] === "kept-by-browser"),
      "the kept-by-browser outcome must be recorded",
    );
    assert.strictEqual(fx.pending.size, 0);
  });

  await test("flow: failed re-issue surfaces recovery-required with the URL", async () => {
    const fx = fakeFx({ cancel: true, downloadFails: true });
    const client = fakeClient({
      prepare: { status: "unavailable" },
      query: { status: "unavailable" },
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "kept-by-browser");
    assert.ok(
      fx.calls.some(
        (c) => c[0] === "record" && c[1] === "recovery-required" && c[2] === "https://files.example.com/pub/report.pdf",
      ),
      "a download that could not be re-issued must be surfaced, never dropped",
    );
    assert.strictEqual(fx.calls.filter((c) => c[0] === "download").length, 1);
  });

  await test("restart with a prepared record resumes by query with the same tx", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      query: [{ status: "prepared" }, { status: "committed", task_ref: "55" }],
      prepare: { status: "prepared" },
    });
    const intent = H.buildIntent(item());
    const out = await H.createHandoffFlow(client, fx).resume({
      tx: "tx-same",
      stage: "prepared",
      intent,
    });
    assert.strictEqual(out, "handed-off");
    assert.deepStrictEqual(
      client.sent.map((s) => [s.op, s.tx]),
      [
        ["query", "tx-same"],
        ["query", "tx-same"],
      ],
    );
    assert.ok(fx.calls.some((c) => c[0] === "sleep" && c[1] === H.POLL_MS));
    assert.ok(!fx.calls.some((c) => c[0] === "cancel" || c[0] === "erase" || c[0] === "download"));
    assert.ok(!client.calls.includes("prepare"));
    assert.ok(!client.calls.includes("commit"));
    assert.ok(fx.calls.some((c) => c[0] === "record" && c[1] === "handed-off" && c[3] === "55"));
    assert.strictEqual(fx.pending.size, 0);
  });

  await test("restart at cancelled prepares the same tx and does not cancel again", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      query: [{ status: "not-found" }, { status: "committed", task_ref: "3" }],
      prepare: { status: "prepared" },
    });
    const out = await H.createHandoffFlow(client, fx).resume({
      tx: "tx-c",
      stage: "cancelled",
      intent: H.buildIntent(item()),
    });
    assert.strictEqual(out, "handed-off");
    assert.ok(!fx.calls.some((c) => c[0] === "cancel" || c[0] === "erase" || c[0] === "download"));
    const prep = client.sent.find((s) => s.op === "prepare");
    assert.strictEqual(prep.tx, "tx-c");
    assert.strictEqual(prep.browser_cancelled, true);
    assert.ok(client.sent.filter((s) => s.op === "query").every((s) => s.tx === "tx-c"));
  });

  console.log(`\n${passed} tests passed`);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
