# Security

Please report a vulnerability privately through GitHub: the **Security** tab of this repository > **Report a vulnerability**. Do not open a public issue for it.

What the connector holds and does, so you know what matters:

- A long-lived device credential from pairing, kept only in Windows Credential Manager or the macOS Keychain, and short-lived relay tickets in memory. Neither is ever logged.
- It reads your own simulator (SimConnect for MSFS, the local X-Plane web API or UDP) and never writes to it.
- It opens no listening port. Its only connections are outbound: the SkyNexus web app (pairing and tickets) and the relay, over TLS.
- On Windows it keeps one entry in MSFS's `exe.xml` ("Start with MSFS"), and never touches the rest of that file.
- `SimConnect.dll` is loaded only from the folder of the running exe, by absolute path. The copy in `src-tauri/vendor/` is the MSFS SDK's, pinned by SHA-256 in the release workflow.
