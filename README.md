# SkyNexus Connector

An optional tray app for [SkyNexus](https://skynexus.nl), the EFB for flight-sim pilots. It reads your own aircraft (and the traffic the sim knows around you) from MSFS 2020/2024 or X-Plane 11/12 on this computer and streams it, about once a second, to your own SkyNexus enroute map. Without it, the map keeps matching you on the VATSIM feed as before.

**Download:** the latest installer is on the [Releases](https://github.com/JoeriSmits/skynexus-connector/releases/latest) page (Windows `-setup.exe`, per-user, no admin rights; macOS `.dmg` for X-Plane). Installed copies update themselves.

Tauri 2, Rust core, tray only (no window). Windows first; the macOS build covers X-Plane. MIT licensed.

What it does not do: install anything in the sim, write to the sim, open a listening port, scan processes, or store frames. Connections are outbound only: the SkyNexus web app (pairing and tickets) and the relay. The one file of the sim's it touches is MSFS's `exe.xml` add-on list, for "Start with MSFS" (see below), and only its own entry there.

## Dev quick start

Needs Rust stable (`rustup default stable`) and Node 22.

```sh
npm install                 # only the Tauri CLI, local to this folder

# A whole flight without a sim, without a network: frames as JSON on stdout
npm run replay              # = --headless --offline --replay src-tauri/fixtures/eddf-departure.json

# The tray app on a replay, against a local web app and relay
cd src-tauri
cargo run -- --replay fixtures/eddf-departure.json --base-url http://localhost:3000 --relay-url ws://localhost:8003 --ephemeral --print-frames
```

Tests and lint:

```sh
npm test                    # cargo test
npm run lint                # cargo clippy --all-targets -- -D warnings
npm run build:app           # release .app (macOS) without the DMG step
CI=true npm run build       # all bundles; CI=true skips the Finder styling the DMG step needs a GUI for
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, clippy and the tests on Windows for every pull request.

The Windows-only SimConnect code type-checks from a Mac with `rustup target add x86_64-pc-windows-msvc` and `cargo check --target x86_64-pc-windows-msvc`, but the Tauri build script wants `llvm-rc` for the Windows resource file. Install LLVM, or put a stub named `llvm-rc` on `PATH` for a check-only run (it never links).

## Flags

| Flag | Meaning |
|---|---|
| `--replay <file>` | Play a recording instead of looking for a sim. Loops unless `--replay-once`. `--replay-speed 10` plays faster. |
| `--headless` | No tray. Logs the tray lines to stderr and pairs on the command line (prints the link and code). |
| `--offline` | Never contact SkyNexus or the relay; implies `--print-frames`. |
| `--print-frames` | Every frame as one JSON line on stdout. Output can be replayed with `--replay`. |
| `--base-url <url>` | Web app for pairing and tickets. Default `https://app.skynexus.nl`. |
| `--relay-url <url>` | Relay base (`/v1/connector/ws` is added). Default `wss://charts.skynexus.nl`; a build can bake another in with `SKYNEXUS_CONNECTOR_RELAY_URL`. |
| `--ephemeral` | Keep the credential in memory, not in the OS keychain. |
| `--no-browser` | Do not open the browser during pairing. |
| `--hz <1-2>` | Own-aircraft frame rate. Default 1 (the relay allows at most 5 frames per second). |
| `--launched-by-msfs` | Set by the `exe.xml` entry: quit once MSFS has closed. |

The keychain entry is per web-app host, so a dev `--base-url` never touches the production credential. Release builds accept plain `http://` and `ws://` only for localhost; anything else must be `https://` or `wss://`, and a relay URL handed out in a ticket must be `wss://`. Release builds on Windows have no console, so use a debug build for `--headless` there.

## How it works

1. **Pairing** (RFC 8628 in shape; a busy or rate-limited start is waited out, and an evicted code starts over once). `POST /api/connector/pair/start` gives a code; the browser opens `/connect?code=ABCD-EFGH`; the signed-in user clicks Allow; `POST /api/connector/pair/poll` (honouring `interval` and `slow_down`) returns a long-lived `snc_...` credential, kept in Windows Credential Manager or the macOS Keychain. The code also shows in the tray, for a browser on another device.
2. **Tickets.** The credential buys a 15-minute ES256 ticket at `POST /api/connector/ticket`, refreshed 90 s before expiry. Expiry runs on the monotonic clock from `expires_in`, so a PC with a wrong clock still works; the JWT time is only used to warn about the clock. `Retry-After` on 429 and 503 is honoured. 401 means revoked (sign out); 403 `not_enabled` keeps the credential and asks again every 5 minutes.
3. **Relay.** WebSocket to `/v1/connector/ws`; the first message is `{"type":"auth","ticket":...}`, then the relay acks `ready` and the connector sends bare frames. At the refresh time it sends the new ticket as another auth message on the same socket and the relay acks `ready` again, so the stream never stops for a refresh; a relay that does not ack within 5 s (one without renewal) gets a reconnect with that ticket. The ticket never goes in the URL (the relay answers `?ticket=` with HTTP 400). Backoff with jitter; close 4001 refreshes the ticket at once, 4003 fetches a new one (it also means the clock is off), 4029 waits 45 s, 1009 means a frame was too big; an upgrade answered 429 or 503 backs off from 5 s to 60 s.
4. **Frames:** `{v:1, t, sim, own:{lat, lon, altFt, gsKt, hdg, trk, vsFpm, onGround, callsign?}, traffic?:[{id, callsign?, lat, lon, altFt, gsKt, hdg, onGround?}]}`. Own position goes out when it changes, at most `--hz` per second, with a 5 s heartbeat while parked (the map calls a stream stale after 10 s). Traffic rides along at most every 2 s: the nearest 150 (the relay rejects more), cut further to stay under 15 KB (the relay's limit is 16 KB). Target ids and callsigns are trimmed printable ASCII; a non-ASCII callsign is left out rather than mangled.
5. **Sign out** revokes the device (`DELETE /api/connector/devices` with the credential) and forgets the credential.

All web app request and response shapes live in `src-tauri/src/api.rs`; the relay protocol in `src-tauri/src/relay.rs`. Frames only ever go from your connector to your own map: the relay forwards them to the browser tabs of the same account and never back to a connector.

## Simulators

Auto-detect tries, every 5 s until one answers: MSFS, the X-Plane web API, X-Plane UDP.

- **MSFS 2020/2024 (Windows):** SimConnect, with `SimConnect.dll` loaded at runtime from an absolute path beside the exe only (`LoadLibraryExW` with `LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`; never the current directory or `PATH`). Without the DLL the tray says "SimConnect.dll missing next to the app; reinstall". The DLL is not in this repo (it comes with the MSFS SDK); the release workflow places it next to the exe. Own aircraft once per second with the changed-only flag (every 10th visual frame at `--hz 2`); traffic (aircraft and helicopters within 80 km) every 2 s. vPilot never sets the on-ground flag for injected aircraft, so it is inferred from height above ground and speed. Every 2 s it also asks the "Sim" system state: 1 is a flight, 0 the menus or loading (tray: "MSFS running, no flight loaded", nothing streamed). MSFS 2024 answers nothing at all while it starts, and a connection opened too early can stay mute for good, so one with no "Sim" answer 45 s after opening is replaced; after that, 15 s of silence in a flight or 120 s outside one counts as gone.
- **X-Plane 12.1.1+:** the built-in web API on `localhost:8086`. If X-Plane answers 403, untick "Disable incoming traffic" in Settings > Network. Traffic comes from the TCAS target datarefs that xPilot (XPMP2), LiveTraffic and X-Plane AI fill.
- **X-Plane 11 and 12.0:** UDP RREF to port 49000 from an ephemeral local port. Traffic from the 19 legacy multiplayer slots, without callsigns.

## Start with MSFS (Windows)

MSFS 2020 and 2024 start the add-on programs listed in `exe.xml` (FSUIPC, GSX and Fenix use it too). On every normal start the tray app makes sure that file holds one entry named `SkyNexus Connector` pointing at the running exe with `--launched-by-msfs`, in each MSFS install it finds: `%APPDATA%\Microsoft Flight Simulator[ 2024]\` (Steam) and `%LOCALAPPDATA%\Packages\Microsoft.FlightSimulator_8wekyb3d8bbwe\LocalCache\` or `...\Microsoft.Limitless_8wekyb3d8bbwe\LocalCache\` (Microsoft Store). An existing `exe.xml` is updated; a new one is only created in a folder that holds MSFS's `UserCfg.opt`, so a folder left behind by an uninstalled MSFS stays untouched.

- On by default; the tray's "Start with MSFS" turns it off (the entry is removed) and on. The choice lives in `start-with-msfs` in the app's config folder (`%APPDATA%\nl.skynexus.connector`).
- Only that entry is added, updated (a moved exe) or removed. Everything else in the file stays byte for byte, the previous file is kept as `exe.xml.skynexus-backup` before each change, and a file without `</SimBase.Document>` is left alone.
- Replays, `--offline` and a dev `--base-url` never touch `exe.xml`.
- Started by MSFS, the connector quits 30 s after MSFS has gone (after having been connected at least once; before that it waits however long MSFS takes to start). Started by hand or at login, it keeps running.
- If a connector is already running, the copy MSFS starts exits at once (single instance). The running one then takes over: it quits when MSFS closes, unless "Start at login" started it (`--at-login`), which keeps running.
- Uninstalling removes the entry: the installer's `PREUNINSTALL` hook (`src-tauri/windows/hooks.nsh`) runs the exe with `--remove-msfs-entry`, which does only that and exits.

All of it is in `src-tauri/src/msfs_launch.rs`.

## Logs

Release builds on Windows have no console, so every log line also goes to `connector.log`: `%LOCALAPPDATA%\nl.skynexus.connector\logs` on Windows, `~/Library/Logs/nl.skynexus.connector` on macOS. Past 5 MB it is rotated at start-up (three files kept). "Open logs" in the tray writes `diagnostics.txt` there (version, OS, status, sim and relay state, the start options and which `exe.xml` files hold the entry, the last 300 log lines) and opens the folder; that file is what to attach to a bug report. No credential, ticket, pairing code or frame is ever logged. `RUST_LOG` still sets the level.

## Replay files

Either a document, `{"description": "...", "samples": [{"t": <ms from start>, "own": {...}, "traffic"?: [...]}]}`, or JSON Lines of frames as `--print-frames` writes them. The checked-in `fixtures/eddf-departure.json` is synthetic (a made-up runway 25C departure; callsigns `DEMO01` and `TEST11` to `TEST33`), and a test fails if a fixture carries any other callsign. Keep real recordings out of the repo.

## Layout

```
package.json              Tauri CLI only
scripts/                  release config for CI (release-config.mjs) and its tests
ui/index.html             placeholder frontendDist (the app has no window)
.github/workflows/        ci.yml (tests on every PR), release.yml (v* tags)
src-tauri/
  tauri.conf.json         bundle, NSIS per-user install, updater (public key placeholder)
  windows/hooks.nsh       uninstaller hook: removes the exe.xml entry
  fixtures/               synthetic replay
  icons/
  src/
    main.rs, lib.rs       entry, logging, headless mode, Tauri builder, updater loop
    config.rs             flags, base and relay URLs
    api.rs                web app routes and shapes
    pairing.rs            start, open browser, poll, store
    credentials.rs        keychain and memory stores
    ticket.rs             ticket cache and refresh
    relay.rs              WebSocket uplink, auth, renewal, backoff, close codes
    frame.rs              frame schema, validation, size budget
    engine.rs             lifetimes of the sim thread, pairing and relay; tray status lines
    tray.rs               tray menu
    msfs_launch.rs        Start with MSFS: the exe.xml entry, quit when MSFS closes
    logs.rs               the log file, rotation, diagnostics.txt
    sim/
      mod.rs              Sim trait, auto-detect, pacing driver
      replay.rs           replay sim
      xplane.rs           shared dataref handling
      xplane_web.rs       X-Plane web API
      xplane_udp.rs       X-Plane UDP RREF
      msfs_proto.rs       SimConnect constants, layouts, decoding (all platforms, tested)
      msfs.rs             SimConnect FFI (Windows only)
```

## Updates and releases

The Tauri updater checks every 6 hours, downloads in the background and installs only while no sim is connected, or on Quit. Checks run only in release builds (debug builds never check). The public key in `tauri.conf.json` is the real one; its private half signs the releases in CI.

Releases come from `.github/workflows/release.yml` on `v*` tags: `scripts/release-config.mjs` writes `src-tauri/tauri.release.conf.json` (updater artifacts on, the real public key from the `TAURI_UPDATER_PUBKEY` variable, `SimConnect.dll` beside the exe on Windows, Authenticode only when configured), and `tauri-action` uploads to a draft release. `npm run test:release` tests that script. The checklist is in [RELEASING.md](RELEASING.md).

## Security

See [SECURITY.md](SECURITY.md) for how to report a vulnerability.

## Licence

[MIT](LICENSE).
