# UDM Handoff — browser extension

Hands browser downloads to [UDM](../README.md) instead of downloading
them in the browser, using the frozen handoff contract: the extension
asks UDM to *prepare* a handoff, cancels the browser download only after
UDM has durably recorded the intent, then *commits* — UDM creates exactly
one download task. If UDM is not running or anything is unconfirmed, the
browser simply keeps its download.

## Layout

| File | Purpose |
| --- | --- |
| `manifest.json` | Chrome / Chromium MV3 manifest |
| `manifest.firefox.json` | Firefox MV3 manifest (fixed add-on id `udm-handoff@cino.pw`) |
| `handoff.js` | Contract logic: eligibility, intent building, the wire client, and the per-download state machine. No browser APIs — unit-tested under Node. |
| `background.js` | Browser wiring: `downloads` interception, native messaging, storage. |
| `popup.*` | Toolbar popup: auto-capture toggle + recent handoffs. |
| `options.*` | Settings: auto-capture, exclusions, and the exact registration command for this extension. |
| `strings.js` | EN/VI strings. |
| `tests/handoff.test.js` | `node tests/handoff.test.js` |

## Install (developer mode)

1. Build UDM; the `udm-host` binary sits next to `udm.exe`.
2. Load the extension unpacked:
   - **Chrome:** `chrome://extensions` → Developer mode → *Load unpacked* →
     select this folder (it reads `manifest.json`).
   - **Firefox:** copy this folder, replace `manifest.json` with
     `manifest.firefox.json`, then `about:debugging` → *This Firefox* →
     *Load Temporary Add-on* → select the copy's `manifest.json`.
3. Register the native host once, from a terminal — this is a disclosed,
   user-level opt-in (HKCU), nothing registers itself:

   ```
   udm-host --register chrome <extension-id>
   udm-host --register firefox udm-handoff@cino.pw
   ```

   The extension's Options page shows the exact command with this
   installation's extension id filled in. Undo with
   `udm-host --unregister <chrome|firefox>`.
4. In UDM, set a handoff lease (`handoff_lease_secs` in the settings
   file) — until a lease is configured the coordinator never starts and
   the browser keeps every download.

## Behaviour, in the contract's terms

- **UDM not running / host missing** → the coordinator answers
  `unavailable` or cannot be reached; the browser download proceeds
  untouched.
- **Cancel unconfirmed** (the download finished first) → the handoff is
  aborted; the browser copy is the file.
- **Commit after the lease expired** → the popup shows *Needs recovery in
  UDM*; nothing is silently re-downloaded.
- **Auto-capture** is on while the extension is enabled, minus your
  exclusions (host, `*.host` suffix, `.ext`, or URL substring).

## Privacy

No telemetry, no network traffic from the extension itself: it talks
only to the local `udm-host` process via Native Messaging. It never
sends cookies, headers, or page content — only the download's URL,
filename/size hints, and the browser's download id, exactly the intent
the handoff contract defines.
