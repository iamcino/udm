# UDM Handoff: browser extension

**Send eligible browser downloads to the running UDM app.** The Chrome and
Firefox extension uses a local Native Messaging host, `udm-host`, and a
durable prepare/commit protocol to transfer download ownership.

This guide describes `241f550`. Setup is currently manual: the regular
installer and portable ZIP do not include the host or extension. The
extension passes a GET URL and hints, not cookies or a browser login session.
Some signed-in or session-dependent downloads therefore cannot work in UDM.

## Before you start

- Windows 11 with a build of UDM and `udm-host` from the same source tree.
- Chrome/Chromium developer-mode loading, or Firefox temporary add-on loading.
- A stable user-writable host location and the same resolved application data
  root for UDM and the host.

From the repository root, build all three binaries:

```powershell
cargo build --release --locked --no-default-features -p udm-gui -p udm-cli -p udm-host
```

They are under `target\release\`. For a portable app, keep the host beside
it with the same `udm.portable` marker, or explicitly give both processes the
same `UDM_DATA_DIR` environment. A per-user app and a host in an unrelated
portable directory will otherwise look for different pairing files.

## Load and register one browser

1. **Load the extension.** In Chrome, open `chrome://extensions`, enable
   Developer mode, choose **Load unpacked** and select this `extension`
   folder. For Firefox, copy the folder to a separate location, replace
   the copy's `manifest.json` with `manifest.firefox.json`, then select that
   manifest through `about:debugging` → **This Firefox** → **Load Temporary
   Add-on**. Keep the repository's Chrome manifest unchanged. Firefox's
   temporary installation is for testing and must be loaded again after a
   browser restart.
2. **Read the registration command** on the extension's Options page. It
   includes that installation's actual extension ID.
3. **Run the command using the host you intend to keep.** For example, from
   its directory in PowerShell:

   ```powershell
   .\udm-host.exe --register chrome <extension-id>
   ```

   Or, for the Firefox manifest's fixed ID:

   ```powershell
   .\udm-host.exe --register firefox udm-handoff@cino.pw
   ```

   Replace `<extension-id>` with the Options value. These are alternatives
   for a single host location; see the identity limitation below before
   registering both browsers.
4. **Restart UDM after registration.** Its coordinator reads the caller
   list on startup. Keep UDM running while handing downloads to it.
5. **Try a public direct file link.** Check both UDM's task list and the
   extension popup's recent outcomes.

No administrator prompt or service is needed. Registration writes a browser
host manifest under `<data>\handoff\`, its HKCU Native Messaging host key,
the caller allow-list, and `udm-host.caller` beside the executable. It does
not happen silently at app start. Moving the host requires re-registration.

Fresh settings already have a **60-second** handoff lease. A positive lease
and a nonempty registered caller list are both required to start the
coordinator. An explicitly stored `null` lease stays disabled for existing
installations. There is no separate settings file or supported CLI setter
for this lease at this revision; do not look for the configuration-file step
described by older versions of this guide.

## Capture and exclusions

Auto-capture defaults on while the extension is enabled. You can turn it
off in the popup or Options and enter exclusions, one per line:

| Rule | Matching behaviour |
|---|---|
| `example.com` | Exact host, and the general URL-substring rule also applies. |
| `*.example.com` | The host itself and its subdomains. |
| `.zip` | Browser-reported filename suffix. |
| `/private-download/` | Case-insensitive substring of the URL. |

Only in-progress HTTP/HTTPS downloads are eligible. Excluded items, finished
items and other URL schemes stay with the browser. Rules are case-insensitive;
host-looking text is not an exact-host-only exclusion when it also occurs
elsewhere in a URL.

## How ownership moves

```text
Browser download → pause → prepare → durable acknowledgement
                                       │
                       confirm browser cancellation
                                       │
                                     commit
                                       │
                       durable commitment → associated Usagi task
```

The coordinator keys a transaction by registered caller and opaque
transaction ID. It durably records prepare before acknowledging. The
extension requests commit only after browser cancellation is confirmed.
Commit records the commitment before asking the engine to create a task.
The engine's persisted `handoff_tx` marker lets recovery find a task whose
association reply was lost, avoiding duplicate materialization for that
transaction. An expired prepared record does not authorize automatic transfer.

The host uses length-prefixed JSON on stdin/stdout and a paired per-user
named pipe. It forwards coordinator states rather than inventing a successful
download. It does not start the GUI, export a network listener or remotely
control an arbitrary engine task.

## Recovery and current limits

| Situation | What the current implementation does |
|---|---|
| App/host unavailable before prepare succeeds | Leaves/resumes the browser download. |
| Browser cancellation cannot be confirmed | Does not commit; sends a best-effort abort. A browser copy that already completed remains the file. |
| Commit arrives after lease expiry | Returns `recovery-required`; no automatic task start. The extension records a recovery outcome. |
| Commitment is durable but task association is pending | Keeps recovery evidence; the extension can report recovery and UDM shows a read-only Needs recovery row in All. |
| App restarts with pending committed records | Reconciles them against engine transaction markers and creates/finds the associated task where possible. |

**Post-cancellation reporting gap:** if commit returns another failure or
`unavailable`, the current extension records `kept-by-browser` even though
the browser download may already have been cancelled and its history entry
erased. That label is not proof the browser still has the file. Check UDM's
task list and journal before manually starting another copy. This guide
does not promise automatic recovery for every lost reply.

The extension's recovery outcome and the GUI's recovery row are different:
the GUI lists pending committed associations, not every expired prepared
transaction. Those rows have no task actions. There is no automatic retry
or user-facing recovery workflow for every outcome at this revision.

**One identity sidecar per host directory:** registration stores a single
`udm-host.caller`. Registering Chrome then Firefox beside the same host
overwrites that identity; the earlier registration cannot be assumed to
retain its original caller. Use one browser per host location. Testing
both requires separate host directories/sidecars and a shared, explicit
`UDM_DATA_DIR` visible to the browser-launched hosts and UDM. Do not treat
simultaneous registration beside one executable as isolated browser identities.

## Disable or remove the integration

Turn off capture first, then run the matching command from the registered
host's directory using the same data root:

```powershell
.\udm-host.exe --unregister chrome
# Or, for the Firefox registration:
.\udm-host.exe --unregister firefox
```

Unregistration removes that browser's HKCU key and caller-list entries; it
removes the local sidecar if it names that browser. It does not erase the
transaction journal, host log or all generated manifest files. Restart UDM
to reload caller availability, then disable/remove the extension in the
browser. The current app uninstaller does not perform this cleanup for you.

## Privacy

The extension has `downloads`, `nativeMessaging` and `storage` permissions.
It sends URL, GET method, filename/size hints and browser download ID locally;
it does not read page content or forward cookies/authentication headers.
Its own code makes no external HTTP requests. UDM's engine makes the actual
file requests after commitment.

Capture settings/exclusions and up to 20 recent outcomes stay in
`storage.local`. A recent entry may contain a filename or URL fallback.
The coordinator journal also stores intent URLs and recovery evidence under
the data root. These records can be sensitive. See [Privacy](../docs/PRIVACY.md).

## Source map and checks

| File | Responsibility |
|---|---|
| `manifest.json`, `manifest.firefox.json` | Browser manifests; Firefox ID `udm-handoff@cino.pw`. |
| `handoff.js` | Eligibility, intent, wire client and ownership flow without browser APIs. |
| `background.js` | Download interception, Native Messaging and browser-local state. |
| `popup.*`, `options.*`, `strings.js` | Capture controls, recent outcomes, exclusions, registration command and EN/VI strings. |
| `tests/handoff.test.js` | Node tests for the shared flow and exclusions. |

Run `node extension/tests/handoff.test.js` from the repository root.
Core handoff and Windows pipe tests live under `crates/core/tests/`.
Unit tests do not replace a real browser/host/GUI run, especially for native
registration, browser cancellation and restart recovery.
