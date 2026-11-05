# Releasing

Releases are built by `.github/workflows/release.yml` on a `vX.Y.Z` tag. It builds Windows, then macOS, on GitHub-hosted runners and uploads the installers, the updater signatures and a merged `latest.json` to a **draft** release. Publishing the draft is the release switch: installed copies only see published releases (`/releases/latest/download/latest.json`).

## One-time setup

### 1. Updater key

```sh
npm ci
npm run tauri signer generate -- -w ~/.tauri/skynexus-connector.key
```

- Choose a password. Keep the key file and the password in a password manager **and** an offline backup: losing either means installed copies can never update again.
- Repository Settings > Secrets and variables > Actions:
  - secret `TAURI_SIGNING_PRIVATE_KEY`: the contents of `~/.tauri/skynexus-connector.key`;
  - secret `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: the password;
  - variable `TAURI_UPDATER_PUBKEY`: the contents of `~/.tauri/skynexus-connector.key.pub` (public; it may also replace `REPLACE_WITH_UPDATER_PUBLIC_KEY` in `src-tauri/tauri.conf.json`).
- While the placeholder is in a build, that build never checks for updates (by design).

### 2. SimConnect.dll (Windows build)

Take it from the MSFS SDK (`MSFS SDK/SimConnect SDK/lib/SimConnect.dll`). It is not in this repository. The MSFS 2024 SDK's DLL is 58 368 bytes, about 78 KB in base64, over GitHub's 48 KB secret limit, so:

- upload it somewhere private (for example R2 with a long-lived signed URL), set secret `SIMCONNECT_DLL_URL`, and variable `SIMCONNECT_DLL_SHA256` = `shasum -a 256 SimConnect.dll`.

(`SIMCONNECT_DLL_BASE64` is also supported, for a DLL that fits.) The workflow checks the download is a Windows DLL with that hash, and stops with a clear error when neither is set.

### 3. Code signing (optional, recommended)

Unsigned works, with SmartScreen and Gatekeeper warnings.

- **SignPath Foundation** (free for open-source projects under an OSI licence such as this one's MIT): apply at signpath.org, then sign through their GitHub Action or `bundle.windows.signCommand`.
- **OV certificate**: secret `WINDOWS_CERTIFICATE` (the PFX, base64) and `WINDOWS_CERTIFICATE_PASSWORD`. New OV keys live on a hardware token or cloud HSM, so an exportable PFX may not exist; then use the CA's signing tool through `bundle.windows.signCommand`.
- **Azure Artifact Signing**: secrets `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TENANT_ID`; variables `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_CERT_PROFILE`.
- **macOS** (Apple Developer Program): secrets `APPLE_CERTIFICATE` (base64 .p12), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`; for notarization also `APPLE_ID`, `APPLE_PASSWORD` (app-specific), `APPLE_TEAM_ID`.

## Each release

1. Bump `version` in `src-tauri/tauri.conf.json` (and `Cargo.toml`), merge to `main`.
2. Optional dry run: Actions > release > Run workflow, without "release". It builds both platforms and keeps the files as workflow artifacts.
3. Tag and push:

   ```sh
   git checkout main && git pull
   git tag v0.1.0
   git push origin v0.1.0
   ```

   The tag must match the version (`v` + `version`), or the workflow stops.
4. Download the draft's installer, install it over the current one, pair, fly. Only then publish the draft: that is the moment installed copies can see it.
