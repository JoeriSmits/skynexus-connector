#!/usr/bin/env node
// Writes src-tauri/tauri.release.conf.json, merged over tauri.conf.json by
// `tauri build --config` in .github/workflows/release.yml.
//
//   node scripts/release-config.mjs <windows|macos>
//
// It turns on the updater artifacts with the real public key, bundles
// SimConnect.dll beside the exe on Windows, and adds Authenticode signing
// only when its settings exist. Nothing secret is written: the updater key
// is public, and certificates stay in the runner's store or in Azure.
//
// Environment:
//   TAURI_UPDATER_PUBKEY     the minisign public key (repository variable); falls back to
//                            tauri.conf.json when that no longer holds the placeholder
//   WINDOWS_CERT_THUMBPRINT  set by the workflow after importing an OV certificate (PFX)
//   WINDOWS_TIMESTAMP_URL    optional, default http://timestamp.digicert.com
//   AZURE_SIGNING_ENDPOINT, AZURE_SIGNING_ACCOUNT, AZURE_CERT_PROFILE
//                            Azure Artifact Signing (Trusted Signing), used when no thumbprint

import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tauriDir = join(here, "..", "src-tauri");

export const PLACEHOLDER_PREFIX = "REPLACE";

/** A value that may go into a sign command without quoting surprises. */
const SAFE = /^[A-Za-z0-9.:/_-]+$/;

export function releaseConfig(platform, env, baseConfig, simconnectPresent) {
  if (platform !== "windows" && platform !== "macos") {
    throw new Error(`platform must be windows or macos, got ${platform}`);
  }
  const fromConf = baseConfig?.plugins?.updater?.pubkey ?? "";
  const pubkey = (env.TAURI_UPDATER_PUBKEY ?? "").trim() || (fromConf.startsWith(PLACEHOLDER_PREFIX) ? "" : fromConf);
  if (!pubkey) {
    throw new Error(
      "No updater public key: set the repository variable TAURI_UPDATER_PUBKEY (the contents of the .key.pub file " +
        "from `npm run tauri signer generate`), or put it in tauri.conf.json.",
    );
  }
  const conf = { bundle: { createUpdaterArtifacts: true }, plugins: { updater: { pubkey } } };
  if (platform === "windows") {
    if (!simconnectPresent) {
      throw new Error("src-tauri/vendor/SimConnect.dll is missing; the workflow's SimConnect step must run first.");
    }
    conf.bundle.resources = { "vendor/SimConnect.dll": "SimConnect.dll" };
    const thumbprint = (env.WINDOWS_CERT_THUMBPRINT ?? "").trim();
    const endpoint = (env.AZURE_SIGNING_ENDPOINT ?? "").trim();
    const account = (env.AZURE_SIGNING_ACCOUNT ?? "").trim();
    const profile = (env.AZURE_CERT_PROFILE ?? "").trim();
    if (thumbprint) {
      if (!/^[A-Fa-f0-9]{40}$/.test(thumbprint)) throw new Error("WINDOWS_CERT_THUMBPRINT is not a SHA-1 thumbprint");
      conf.bundle.windows = {
        certificateThumbprint: thumbprint,
        digestAlgorithm: "sha256",
        timestampUrl: (env.WINDOWS_TIMESTAMP_URL ?? "").trim() || "http://timestamp.digicert.com",
      };
    } else if (endpoint || account || profile) {
      if (![endpoint, account, profile].every((v) => v && SAFE.test(v))) {
        throw new Error("Azure Artifact Signing needs AZURE_SIGNING_ENDPOINT, AZURE_SIGNING_ACCOUNT and AZURE_CERT_PROFILE (letters, digits, . : / _ - only)");
      }
      conf.bundle.windows = {
        signCommand: `artifact-signing-cli -e ${endpoint} -a ${account} -c ${profile} -d SkyNexusConnector %1`,
      };
    }
  }
  return conf;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  try {
    const base = JSON.parse(readFileSync(join(tauriDir, "tauri.conf.json"), "utf8"));
    const conf = releaseConfig(
      process.argv[2],
      process.env,
      base,
      existsSync(join(tauriDir, "vendor", "SimConnect.dll")),
    );
    const out = join(tauriDir, "tauri.release.conf.json");
    writeFileSync(out, JSON.stringify(conf, null, 2) + "\n");
    const signing = conf.bundle.windows?.certificateThumbprint
      ? "Authenticode with the imported certificate"
      : conf.bundle.windows?.signCommand
        ? "Azure Artifact Signing"
        : "no Authenticode signing";
    console.log(`wrote ${out} for ${process.argv[2]} (${process.argv[2] === "windows" ? signing : "macOS"})`);
  } catch (e) {
    console.error(`::error::${e.message}`);
    process.exit(1);
  }
}
