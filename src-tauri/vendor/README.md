# vendor

## SimConnect.dll

Microsoft's SimConnect client library, which the connector loads at runtime to read MSFS. The Windows installer ships it next to the exe (`scripts/release-config.mjs` adds it as a bundle resource); the connector loads it only from there, by absolute path.

| | |
|---|---|
| Source | MSFS SDK 0.16.0.0, `SimConnect SDK/lib/SimConnect.dll` |
| Size | 58 368 bytes |
| SHA-256 | `677fd3849d54e2a6831dc1aa348d974ead2255ec609fbf5830ef32bf0804b8bc` |
| Authenticode | not signed (as shipped in the SDK) |
| Works with | MSFS 2020 and MSFS 2024 |

**Licence:** this file is Microsoft's and is **not** covered by this repository's MIT licence. It is distributed under the Microsoft Flight Simulator SDK EULA (`Licenses/MSFS SDK EULA.pdf` in the SDK). Other MSFS add-ons ship it the same way (Little Navmap, MobiFlight); whether the EULA allows redistribution has not been confirmed in writing. If Microsoft objects, remove it here and use the workflow's `SIMCONNECT_DLL_URL` option instead (see `RELEASING.md`).

The release workflow checks this file against the SHA-256 above before it builds, so a changed DLL needs this table and the workflow updated together.
