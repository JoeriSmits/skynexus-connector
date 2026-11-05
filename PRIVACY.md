# Privacy

What the SkyNexus Connector sends, where, and how to stop it. It sends nothing until you pair it with your SkyNexus account.

## What leaves your computer

| What | To | When |
|---|---|---|
| Your computer's name (for example `DESKTOP-ABC123`), to label the device in your account | SkyNexus (`app.skynexus.nl`) | Once, when you pair |
| The device credential, to get relay tickets | SkyNexus (`app.skynexus.nl`) | About every 15 minutes while it runs |
| Your own aircraft: position, altitude, ground speed, heading, track, vertical speed, on-ground, and the callsign set in the sim | The SkyNexus relay (`charts.skynexus.nl`), which passes it to **your own** SkyNexus map only | About once a second while a flight is loaded and you are signed in |
| Traffic the sim knows within 80 km (AI, or other pilots from a network client): position, altitude, speed, heading and callsign | The same relay, to your own map only | At most every 2 seconds, with your own position |
| A request for the latest version (no personal data) | GitHub (`github.com`) | Every 6 hours, in release builds |

As with any connection, SkyNexus and GitHub see your IP address. Requests to SkyNexus carry the app version (`SkyNexusConnector/<version>`) and nothing else about your system.

## What it does not do

- The relay does not store frames: they go to your open map tabs and nowhere else, and never to another account.
- No analytics, no telemetry, no crash reports, no advertising.
- It does not read anything from your computer except the simulator data above and its own settings.

## What stays on your computer

- The device credential, in Windows Credential Manager or the macOS Keychain.
- Logs in `%LOCALAPPDATA%\nl.skynexus.connector\logs` (Windows) or `~/Library/Logs/nl.skynexus.connector` (macOS). They hold no credential, ticket, pairing code or position, and they are never uploaded; you share `diagnostics.txt` yourself if you want help.
- Its settings, and on Windows one entry in MSFS's `exe.xml` add-on list ("Start with MSFS").

## How to stop it

- **Pause:** Quit from the tray. Nothing is sent while it is not running or while no flight is loaded.
- **Stop for good:** Sign out from the tray. That revokes the device on SkyNexus and deletes the credential from this computer. You can also remove the device in your SkyNexus account.
- **Start with MSFS** and **Start at login** can be switched off in the tray.
- **Uninstall** removes the app and its `exe.xml` entry.

Questions: open an issue in this repository, or use **Security > Report a vulnerability** for anything sensitive.
