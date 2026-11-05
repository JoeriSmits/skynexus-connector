// npm run test:release
import { test } from "node:test";
import assert from "node:assert/strict";

import { releaseConfig } from "./release-config.mjs";

const placeholder = { plugins: { updater: { pubkey: "REPLACE_WITH_UPDATER_PUBLIC_KEY" } } };
const key = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWdu";

test("refuses to build updater artifacts without a real public key", () => {
  assert.throws(() => releaseConfig("macos", {}, placeholder, false), /TAURI_UPDATER_PUBKEY/);
  const conf = releaseConfig("macos", {}, { plugins: { updater: { pubkey: key } } }, false);
  assert.equal(conf.plugins.updater.pubkey, key, "a real key in tauri.conf.json is enough");
});

test("macOS: updater artifacts and the key, nothing Windows", () => {
  const conf = releaseConfig("macos", { TAURI_UPDATER_PUBKEY: ` ${key} ` }, placeholder, false);
  assert.deepEqual(conf, { bundle: { createUpdaterArtifacts: true }, plugins: { updater: { pubkey: key } } });
});

test("Windows: SimConnect.dll is required and lands beside the exe", () => {
  const env = { TAURI_UPDATER_PUBKEY: key };
  assert.throws(() => releaseConfig("windows", env, placeholder, false), /SimConnect\.dll is missing/);
  const conf = releaseConfig("windows", env, placeholder, true);
  assert.deepEqual(conf.bundle.resources, { "vendor/SimConnect.dll": "SimConnect.dll" });
  assert.equal(conf.bundle.windows, undefined, "unsigned without signing settings");
});

test("Windows: an imported OV certificate signs with SHA-256 and a timestamp", () => {
  const conf = releaseConfig(
    "windows",
    { TAURI_UPDATER_PUBKEY: key, WINDOWS_CERT_THUMBPRINT: "A".repeat(40) },
    placeholder,
    true,
  );
  assert.deepEqual(conf.bundle.windows, {
    certificateThumbprint: "A".repeat(40),
    digestAlgorithm: "sha256",
    timestampUrl: "http://timestamp.digicert.com",
  });
  assert.throws(
    () => releaseConfig("windows", { TAURI_UPDATER_PUBKEY: key, WINDOWS_CERT_THUMBPRINT: "nope" }, placeholder, true),
    /thumbprint/,
  );
});

test("Windows: Azure Artifact Signing builds a sign command from safe values only", () => {
  const env = {
    TAURI_UPDATER_PUBKEY: key,
    AZURE_SIGNING_ENDPOINT: "https://weu.codesigning.azure.net",
    AZURE_SIGNING_ACCOUNT: "skynexus",
    AZURE_CERT_PROFILE: "connector",
  };
  const conf = releaseConfig("windows", env, placeholder, true);
  assert.equal(
    conf.bundle.windows.signCommand,
    "artifact-signing-cli -e https://weu.codesigning.azure.net -a skynexus -c connector -d SkyNexusConnector %1",
  );
  assert.throws(() => releaseConfig("windows", { ...env, AZURE_CERT_PROFILE: "x; rm -rf /" }, placeholder, true), /Azure/);
  assert.throws(() => releaseConfig("windows", { ...env, AZURE_SIGNING_ACCOUNT: "" }, placeholder, true), /Azure/);
});

test("unknown platforms are refused", () => {
  assert.throws(() => releaseConfig("linux", { TAURI_UPDATER_PUBKEY: key }, placeholder, true), /platform/);
});
