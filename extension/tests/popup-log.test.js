"use strict";
/*
 * Popup projection for U7. The page is a view: pending rows come from
 * udmHandoffPending, attention rows from udmHandoffLog, and handed-off
 * history is removed from that log without writing the other keys.
 * Run:  node extension/tests/popup-log.test.js
 */
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

let passed = 0;
async function test(name, fn) {
  await fn();
  passed += 1;
  console.log(`ok - ${name}`);
}

const read = (file) => fs.readFileSync(path.join(__dirname, "..", file), "utf8");
const flush = () => new Promise((r) => setImmediate(r));

function makeEl() {
  const node = {
    className: "",
    title: "",
    hidden: false,
    checked: false,
    children: [],
    append(...kids) {
      this.children.push(...kids);
    },
    addEventListener() {},
  };
  let text = "";
  Object.defineProperty(node, "textContent", {
    get() {
      return text;
    },
    set(v) {
      text = v == null ? "" : String(v);
      node.children = [];
    },
  });
  return node;
}

function loadPopup(store, language) {
  const sets = [];
  const ids = ["title", "enabledLabel", "recentLabel", "optionsLink", "enabled", "log", "none"];
  const els = {};
  for (const id of ids) els[id] = makeEl();
  const api = {
    storage: {
      local: {
        get: async (key) => {
          const keys = Array.isArray(key) ? key : [key];
          const out = {};
          for (const k of keys) {
            if (Object.prototype.hasOwnProperty.call(store, k)) out[k] = store[k];
          }
          return out;
        },
        set: async (obj) => {
          sets.push(JSON.parse(JSON.stringify(obj)));
          for (const [k, v] of Object.entries(obj)) store[k] = JSON.parse(JSON.stringify(v));
        },
      },
    },
    runtime: { openOptionsPage() {} },
  };
  const document = {
    getElementById(id) {
      return els[id];
    },
    createElement() {
      return makeEl();
    },
  };
  const ctx = vm.createContext({
    document,
    navigator: { language },
    console,
    URL,
    setTimeout,
    clearTimeout,
  });
  ctx.chrome = api;
  for (const file of ["strings.js", "handoff.js", "popup.js"]) {
    vm.runInContext(read(file), ctx, { filename: file });
  }
  return { ctx, els, sets };
}

(async () => {
  await test("popup shows pending and recovery-required and drops success history", async () => {
    const store = {
      udmSettings: { enabled: true, exclusions: ["*.example.com"] },
      udmHandoffPending: {
        "tx-live": {
          tx: "tx-live",
          stage: "prepared",
          intent: { filename_hint: "live.bin", url: "https://example.com/live.bin" },
        },
        "tx-rec": {
          tx: "tx-rec",
          stage: "recovery-required",
          intent: { filename_hint: "wait.bin", url: "https://example.com/wait.bin" },
        },
      },
      udmHandoffLog: [
        { tx: "ok", name: "ok.pdf", status: "handed-off", task_ref: "4", at: 1 },
        {
          tx: "k",
          name: "k.bin",
          status: "kept-by-browser",
          reason: "Browser cancellation could not be confirmed",
          at: 2,
        },
        { tx: "bad", name: "bad.pdf", status: "discarded", at: 3 },
        { tx: "tx-rec", name: "wait.bin", status: "recovery-required", at: 4 },
      ],
    };
    const pendingBefore = JSON.parse(JSON.stringify(store.udmHandoffPending));
    const settingsBefore = JSON.parse(JSON.stringify(store.udmSettings));
    const { ctx, els, sets } = loadPopup(store, "en");
    for (let i = 0; i < 10 && els.log.children.length === 0; i++) await flush();

    const T = vm.runInContext("globalThis.UdmStrings.udmT()", ctx);
    const rows = els.log.children.map((li) => ({
      name: li.children[0].textContent,
      status: li.children[1].textContent,
      liClass: li.className,
    }));
    assert.deepStrictEqual(
      rows.map((r) => r.name),
      ["wait.bin", "k.bin", "bad.pdf", "live.bin"],
    );
    assert.strictEqual(rows[0].status, T.status_recovery);
    assert.strictEqual(rows[0].liClass, "needs-action");
    assert.strictEqual(rows[1].status, "Browser cancellation could not be confirmed");
    assert.strictEqual(rows[2].status, T.status_discarded);
    assert.strictEqual(rows[3].status, T.status_pending);
    assert.ok(rows.every((r) => !/save prompt|Ask where/i.test(r.status)));
    assert.ok(!rows.some((r) => r.name === "ok.pdf" || r.status === T.status_handed_off));
    assert.strictEqual(els.none.hidden, true);
    assert.strictEqual(els.recentLabel.textContent, T.recent);
    assert.strictEqual(els.enabled.checked, true);

    assert.deepStrictEqual(
      sets.map((s) => Object.keys(s)),
      [["udmHandoffLog"]],
    );
    assert.deepStrictEqual(
      store.udmHandoffLog.map((e) => e.tx),
      ["k", "bad", "tx-rec"],
    );
    assert.strictEqual(store.udmHandoffLog[2].status, "recovery-required");
    assert.strictEqual(store.udmHandoffLog[2].name, "wait.bin");
    assert.deepStrictEqual(store.udmHandoffPending, pendingBefore);
    assert.deepStrictEqual(store.udmSettings, settingsBefore);
  });

  await test("an empty attention list collapses and does not invent storage", async () => {
    const store = { udmSettings: { enabled: false, exclusions: [] } };
    const { ctx, els, sets } = loadPopup(store, "en");
    for (let i = 0; i < 10 && els.none.textContent === ""; i++) await flush();
    const T = vm.runInContext("globalThis.UdmStrings.udmT()", ctx);
    assert.strictEqual(els.none.hidden, false);
    assert.strictEqual(els.none.textContent, T.none);
    assert.strictEqual(els.log.children.length, 0);
    assert.deepStrictEqual(sets, []);
    assert.strictEqual(els.enabled.checked, false);
    assert.ok(!Object.prototype.hasOwnProperty.call(store, "udmHandoffLog"));
    assert.ok(!Object.prototype.hasOwnProperty.call(store, "udmHandoffPending"));
  });

  await test("vietnamese popup uses the generic recovery wording", async () => {
    const store = {
      udmSettings: { enabled: true, exclusions: [] },
      udmHandoffPending: {
        "tx-rec": {
          tx: "tx-rec",
          stage: "recovery-required",
          intent: { url: "https://example.com/wait.bin" },
        },
      },
    };
    const { ctx, els, sets } = loadPopup(store, "vi-VN");
    for (let i = 0; i < 10 && els.log.children.length === 0; i++) await flush();
    const T = vm.runInContext("globalThis.UdmStrings.udmT()", ctx);
    assert.strictEqual(els.recentLabel.textContent, "Cần chú ý");
    assert.strictEqual(els.log.children[0].children[0].textContent, "https://example.com/wait.bin");
    assert.strictEqual(els.log.children[0].children[1].textContent, T.status_recovery);
    assert.strictEqual(els.log.children[0].className, "needs-action");
    assert.deepStrictEqual(sets, []);
  });

  console.log(`\n${passed} tests passed`);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
