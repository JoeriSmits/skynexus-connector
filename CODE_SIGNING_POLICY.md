# Code signing policy

Windows releases of the SkyNexus Connector are to be signed through SignPath:

> Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org).

**Status:** applied for. Until the application is approved, releases are not Authenticode-signed and Windows SmartScreen may warn. This page will say when signing starts.

## What is signed

Only what this repository builds: `skynexus-connector.exe` and its installer `SkyNexus Connector_<version>_x64-setup.exe`, produced by [`.github/workflows/release.yml`](.github/workflows/release.yml) on GitHub-hosted runners from a `vX.Y.Z` tag of this repository. Nothing built anywhere else is signed.

The installer also contains `SimConnect.dll`, Microsoft's client library for Microsoft Flight Simulator, taken unchanged from the MSFS SDK and checked against a pinned SHA-256 before every build ([`src-tauri/vendor/README.md`](src-tauri/vendor/README.md)). It is not ours and is not signed by us.

## Team and roles

| Role | Who |
|---|---|
| Committers and reviewers | [Joeri Smits](https://github.com/JoeriSmits) |
| Approvers | [Joeri Smits](https://github.com/JoeriSmits) |

- Committers may push to this repository. Changes from anyone else come in as pull requests and are reviewed by a committer before they are merged.
- Every signing request is approved by hand by an approver, for a release built from a tag in this repository.
- Everyone with one of these roles uses multi-factor authentication on GitHub and SignPath.

## Privacy

The connector sends your own simulator aircraft and nearby traffic to your own SkyNexus map, and nothing to anyone else. The details, and how to switch it off, are in the [privacy statement](PRIVACY.md); the installer shows a summary before it installs anything.
