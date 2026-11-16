"use strict";
/*
 * Regression test: the extension background must load in a real browser.
 * Firefox loads handoff.js then background.js as background scripts in ONE
 * shared lexical scope (Chrome shares the worker global lexical scope via
 * importScripts), so a top-level `const api` in both files is a parse-time
 * SyntaxError and the background never runs. The require()-based unit tests
 * load handoff.js standalone and cannot see this; model the shared scope
 * with node:vm instead.
 * Run:  node extension/tests/background-load.test.js
 * No dependencies beyond Node itself.
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

(async () => {
  const createdListeners = [];
  const port = {
    postMessage() {},
    disconnect() {},
    onMessage: { addListener() {} },
    onDisconnect: { addListener() {} },
  };
  const browser = {
    storage: {
      local: {
        get: async () => ({}),
        set: async () => {},
      },
    },
    downloads: {
      onCreated: { addListener: (fn) => createdListeners.push(fn) },
      pause: async () => {},
      resume: async () => {},
      cancel: async () => {},
      search: async () => [],
      erase: async () => {},
    },
    runtime: {
      lastError: undefined,
      connectNative: () => port,
    },
  };
  // Firefox background-script model: no importScripts, one shared scope.
  const context = vm.createContext({ browser });
  const load = (file) =>
    vm.runInContext(fs.readFileSync(path.join(__dirname, "..", file), "utf8"), context, { filename: file });

  await test("the modeled background scope has no importScripts", () => {
    assert.strictEqual(vm.runInContext("typeof importScripts", context), "undefined");
  });

  await test("handoff.js loads in the background scope", () => {
    load("handoff.js");
  });

  await test("background.js loads after handoff.js without a lexical collision", () => {
    load("background.js");
  });

  await test("UdmHandoff is defined on the shared global", () => {
    assert.ok(vm.runInContext("globalThis.UdmHandoff", context));
  });

  await test("exactly one downloads.onCreated listener is registered", () => {
    assert.strictEqual(createdListeners.length, 1);
    assert.strictEqual(typeof createdListeners[0], "function");
  });

  console.log(`\n${passed} tests passed`);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
