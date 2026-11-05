//! Runtime configuration: the SkyNexus base URL, the relay URL and dev flags.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

/// Production web app. Overridable with `--base-url` for dev.
pub const DEFAULT_BASE_URL: &str = "https://app.skynexus.nl";

/// The relay that passes frames to the map. Order of
/// precedence: `--relay-url`, a `relay_url` in the ticket answer (not sent
/// today, kept so a Cloudflare relay can take over without a client
/// release), the build-time `SKYNEXUS_CONNECTOR_RELAY_URL`, then this.
/// Own-aircraft rate ceiling, well under the relay's 5 frames per second.
pub const MAX_HZ: u32 = 2;

pub const DEFAULT_RELAY_URL: &str = "wss://charts.skynexus.nl";
pub const BUILD_RELAY_URL: Option<&str> = option_env!("SKYNEXUS_CONNECTOR_RELAY_URL");

#[derive(Debug, Clone, Parser)]
#[command(
    name = "skynexus-connector",
    version,
    about = "Streams your simulator aircraft to your SkyNexus map"
)]
pub struct Cli {
    /// SkyNexus web app base URL (pairing and tickets).
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// Relay base URL, for example ws://localhost:8787 (the /v1/connector/ws path is added).
    /// Default: wss://charts.skynexus.nl.
    #[arg(long, value_name = "URL")]
    pub relay_url: Option<String>,

    /// Play a recorded flight instead of looking for a simulator.
    #[arg(long, value_name = "FILE")]
    pub replay: Option<PathBuf>,

    /// Play the replay once instead of looping it.
    #[arg(long)]
    pub replay_once: bool,

    /// Replay speed multiplier.
    #[arg(long, default_value_t = 1.0)]
    pub replay_speed: f64,

    /// Run without the tray: log status to the terminal and pair on the command line.
    #[arg(long)]
    pub headless: bool,

    /// Never contact SkyNexus or the relay; only read the sim (implies --print-frames).
    #[arg(long)]
    pub offline: bool,

    /// Print every frame as JSON on stdout.
    #[arg(long)]
    pub print_frames: bool,

    /// Keep the connector credential in memory only (nothing in the OS keychain).
    #[arg(long)]
    pub ephemeral: bool,

    /// Do not open the browser during pairing; print the link instead.
    #[arg(long)]
    pub no_browser: bool,

    /// Own-aircraft frame rate in Hz (1 or 2; the relay allows 5 frames per second).
    #[arg(long, default_value_t = 1)]
    pub hz: u32,

    /// Set by MSFS's exe.xml entry: quit again once MSFS has closed.
    #[arg(long)]
    pub launched_by_msfs: bool,

    /// Set by "Start at login": this copy keeps running when MSFS closes.
    #[arg(long, hide = true)]
    pub at_login: bool,

    /// Remove the "Start with MSFS" entry from every MSFS exe.xml and exit
    /// (the uninstaller runs this).
    #[arg(long, hide = true)]
    pub remove_msfs_entry: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub relay_url_override: Option<String>,
    pub replay: Option<PathBuf>,
    pub replay_loop: bool,
    pub replay_speed: f64,
    pub headless: bool,
    pub offline: bool,
    pub print_frames: bool,
    pub ephemeral: bool,
    pub open_browser: bool,
    pub hz: u32,
    /// Started by MSFS (exe.xml): quit once MSFS has closed.
    pub launched_by_msfs: bool,
    /// Started by "Start at login": never hands itself to MSFS's lifetime.
    pub at_login: bool,
    /// Only remove the exe.xml entry, then exit.
    pub remove_msfs_entry: bool,
    /// Plain http/ws only to this computer (release builds). Debug builds
    /// allow it anywhere, for a dev server on the LAN.
    pub strict_transport: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config::from_cli(Cli::parse_from(["skynexus-connector"]))
    }
}

impl Config {
    pub fn from_cli(cli: Cli) -> Self {
        let strict = !cfg!(debug_assertions);
        let mut base_url = normalise_base(cli.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL));
        if !transport_allowed(&base_url, strict) {
            eprintln!(
                "refusing --base-url {base_url}: use https (plain http only to localhost); using {DEFAULT_BASE_URL}"
            );
            base_url = DEFAULT_BASE_URL.to_string();
        }
        Config {
            base_url,
            strict_transport: strict,
            relay_url_override: cli.relay_url.map(|u| u.trim_end_matches('/').to_string()),
            replay: cli.replay,
            replay_loop: !cli.replay_once,
            replay_speed: if cli.replay_speed.is_finite() && cli.replay_speed > 0.0 {
                cli.replay_speed
            } else {
                1.0
            },
            headless: cli.headless,
            offline: cli.offline,
            print_frames: cli.print_frames || cli.offline,
            ephemeral: cli.ephemeral,
            open_browser: !cli.no_browser,
            hz: cli.hz.clamp(1, MAX_HZ),
            launched_by_msfs: cli.launched_by_msfs,
            at_login: cli.at_login,
            remove_msfs_entry: cli.remove_msfs_entry,
        }
    }

    /// A normal run against production: the only kind that may write the
    /// MSFS exe.xml entry (never a replay, an offline run or a dev web app).
    pub fn is_production_run(&self) -> bool {
        self.replay.is_none() && !self.offline && self.base_url == DEFAULT_BASE_URL
    }

    /// Parses the process arguments. Unknown arguments (an OS launcher can add
    /// some) fall back to the defaults instead of refusing to start.
    pub fn from_env_args() -> Self {
        match Cli::try_parse() {
            Ok(cli) => Config::from_cli(cli),
            Err(e)
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                e.exit()
            }
            Err(e) => {
                eprintln!("ignoring arguments: {e}");
                Config::default()
            }
        }
    }

    pub fn frame_interval(&self) -> Duration {
        Duration::from_millis(1000 / u64::from(self.hz.max(1)))
    }

    /// The relay base URL: the flag, then the ticket answer, then the build, then the default.
    /// A candidate that fails the transport rules is skipped with a warning;
    /// a ticket's `relay_url` must be `wss://`.
    pub fn relay_base(&self, from_ticket: Option<&str>) -> String {
        let strict = self.strict_transport;
        let candidates: [RelayCandidate; 4] = [
            (self.relay_url_override.as_deref(), "--relay-url", transport_allowed),
            (from_ticket, "the ticket's relay_url", |u, _| ticket_relay_allowed(u)),
            (BUILD_RELAY_URL, "the build's relay URL", transport_allowed),
            (Some(DEFAULT_RELAY_URL), "the default relay URL", transport_allowed),
        ];
        candidates
            .into_iter()
            .filter_map(|(u, what, ok)| Some((u?.trim().trim_end_matches('/'), what, ok)))
            .filter(|(u, _, _)| !u.is_empty())
            .find(|(u, what, ok)| {
                let allowed = ok(u, strict);
                if !allowed {
                    log::warn!("ignoring {what} {u}: it must use TLS");
                }
                allowed
            })
            .map(|(u, _, _)| u.to_string())
            .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string())
    }

    /// The keychain account name, so a dev base URL never overwrites the
    /// production credential.
    pub fn keychain_account(&self) -> String {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| match u.port() {
                    Some(p) => format!("{h}:{p}"),
                    None => h.to_string(),
                })
            })
            .unwrap_or_else(|| self.base_url.clone())
    }
}

/// A relay URL source: the URL, what to call it in a warning, its rule.
type RelayCandidate<'a> = (Option<&'a str>, &'static str, fn(&str, bool) -> bool);

fn is_local_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    }
}

/// Whether credentials, tickets or positions may travel to `url`: https and
/// wss always; http and ws only to localhost when `strict` (release builds),
/// anywhere otherwise. Every other scheme is refused.
pub fn transport_allowed(url: &str, strict: bool) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    match u.scheme() {
        "https" | "wss" => u.host().is_some(),
        "http" | "ws" => u.host().is_some_and(|h| !strict || is_local_host(&h)),
        _ => false,
    }
}

/// A relay URL handed out in a ticket answer: `wss://` only, in every build.
pub fn ticket_relay_allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.scheme() == "wss" && u.host().is_some())
}

fn normalise_base(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

/// `{relay}/v1/connector/ws`. Accepts http(s) and ws(s) bases. The ticket
/// goes in the first message, never in the URL.
pub fn relay_socket_url(relay_base: &str) -> Result<url::Url, url::ParseError> {
    let base = relay_base.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    let path = if base.ends_with("/v1/connector/ws") {
        base
    } else {
        format!("{base}/v1/connector/ws")
    };
    url::Url::parse(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(args: &[&str]) -> Config {
        let mut all = vec!["skynexus-connector"];
        all.extend_from_slice(args);
        Config::from_cli(Cli::parse_from(all))
    }

    #[test]
    fn defaults() {
        let c = cfg(&[]);
        assert_eq!(c.base_url, DEFAULT_BASE_URL);
        assert_eq!(c.keychain_account(), "app.skynexus.nl");
        assert_eq!(c.frame_interval(), Duration::from_secs(1));
        assert!(c.replay_loop && c.open_browser && !c.offline);
        assert!(c.is_production_run() && !c.launched_by_msfs);
        assert!(cfg(&["--launched-by-msfs"]).launched_by_msfs);
        assert!(cfg(&["--at-login"]).at_login && !c.at_login);
        assert!(cfg(&["--remove-msfs-entry"]).remove_msfs_entry && !c.remove_msfs_entry);
        assert!(!cfg(&["--offline"]).is_production_run());
        assert!(!cfg(&["--base-url", "http://localhost:3000"]).is_production_run());
    }

    #[test]
    fn dev_overrides() {
        let c = cfg(&[
            "--base-url",
            "http://localhost:3000/",
            "--relay-url",
            "ws://localhost:8787/",
            "--hz",
            "9",
            "--offline",
        ]);
        assert_eq!(c.base_url, "http://localhost:3000");
        assert_eq!(c.keychain_account(), "localhost:3000");
        assert_eq!(c.relay_base(Some("wss://other")), "ws://localhost:8787");
        assert_eq!(c.hz, 2);
        assert_eq!(c.frame_interval(), Duration::from_millis(500));
        assert!(c.print_frames);
    }

    #[test]
    fn relay_precedence_without_flag() {
        let c = cfg(&[]);
        assert_eq!(c.relay_base(Some("wss://relay.example/")), "wss://relay.example");
        if BUILD_RELAY_URL.is_none() {
            assert_eq!(c.relay_base(None), DEFAULT_RELAY_URL);
        }
    }

    #[test]
    fn transport_rules() {
        for ok in [
            "https://app.skynexus.nl",
            "wss://charts.skynexus.nl",
            "http://localhost:3000",
            "ws://127.0.0.1:8787",
            "http://[::1]:3000",
        ] {
            assert!(transport_allowed(ok, true), "{ok}");
        }
        for bad in [
            "http://192.168.1.5:3000",
            "ws://relay.example",
            "ftp://x",
            "not a url",
            "file:///etc",
        ] {
            assert!(!transport_allowed(bad, true), "{bad}");
        }
        assert!(
            transport_allowed("http://192.168.1.5:3000", false),
            "debug builds allow LAN dev"
        );
        assert!(!transport_allowed("ftp://x", false));

        assert!(ticket_relay_allowed("wss://relay.example"));
        for bad in ["ws://localhost:8787", "https://relay.example", "wss://"] {
            assert!(!ticket_relay_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn strict_config_skips_plain_relays() {
        let mut c = cfg(&["--relay-url", "ws://10.0.0.2:8787"]);
        c.strict_transport = true;
        assert_eq!(c.relay_base(None), relay_or_default(), "a plain LAN relay is refused");
        let c = cfg(&[]);
        assert_eq!(
            c.relay_base(Some("ws://evil.example")),
            relay_or_default(),
            "tickets must say wss"
        );
        assert_eq!(c.relay_base(Some("wss://cf.example/")), "wss://cf.example");
        let mut c = cfg(&["--relay-url", "ws://localhost:8787"]);
        c.strict_transport = true;
        assert_eq!(c.relay_base(None), "ws://localhost:8787", "localhost dev still works");
    }

    fn relay_or_default() -> String {
        BUILD_RELAY_URL.unwrap_or(DEFAULT_RELAY_URL).to_string()
    }

    #[test]
    fn socket_url_shapes() {
        let u = relay_socket_url("https://relay.example").unwrap();
        assert_eq!(u.as_str(), "wss://relay.example/v1/connector/ws");
        let u = relay_socket_url("ws://localhost:8787/v1/connector/ws").unwrap();
        assert_eq!(u.as_str(), "ws://localhost:8787/v1/connector/ws");
        assert_eq!(
            relay_socket_url(DEFAULT_RELAY_URL).unwrap().as_str(),
            "wss://charts.skynexus.nl/v1/connector/ws"
        );
    }
}
