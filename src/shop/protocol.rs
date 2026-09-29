//! The o-charts shop wire protocol: building requests and reading replies.
//!
//! Pure. Nothing here opens a socket, so every shape of reply can be tested
//! against a literal.
//!
//! The protocol is not published; it was established by reading the GPL-2.0
//! `o-charts_pi` plugin's behaviour. This is a clean reimplementation from
//! those observed facts — endpoint, field names, reply shape — and shares no
//! code with it.
//!
//! One endpoint, `POST`, `application/x-www-form-urlencoded`, XML back:
//!
//! ```text
//! https://o-charts.org/shop/index.php?fc=module&module=occharts&controller=apioesu
//! ```
//!
//! Every reply is `<response><result>CODE</result>…</response>`, where the code
//! is `1` for success and otherwise names the failure. The code may carry a
//! server message after a colon.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use super::types::{Chart, DownloadGrant, DownloadTarget, Edition, FileGrant, Quantity, Slot};

/// The one endpoint every task goes to.
pub const ENDPOINT: &str =
    "https://o-charts.org/shop/index.php?fc=module&module=occharts&controller=apioesu";

/// What Manx calls itself to the shop.
///
/// The server refuses a client whose `version` it does not recognise (result
/// `5`, "plugin version obsolete"), so this has to be a string it accepts. It
/// is a compatibility gate, not an entitlement check: what may be downloaded is
/// decided server-side from the account, its slots and the machine fingerprint,
/// and none of that is touched here.
///
/// `MANX_SHOP_VERSION` overrides it, which is how to react to a gate change
/// without a release.
pub fn client_version() -> String {
    if let Ok(v) = std::env::var("MANX_SHOP_VERSION") {
        if !v.trim().is_empty() {
            return v;
        }
    }
    // Prefix is the platform: w. Windows, d. macOS, l. Linux, r. Android.
    let os = if cfg!(target_os = "windows") {
        "w."
    } else if cfg!(target_os = "macos") {
        "d."
    } else if cfg!(target_os = "android") {
        "r."
    } else {
        "l."
    };
    format!("{os}{}", super::COMPATIBLE_PLUGIN_VERSION)
}

/// A form body, percent-encoded.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// The password is sent as the uppercase hex of its UTF-8 bytes.
///
/// This is not a hash and gives no protection — it is reversible by
/// inspection. It is what the server expects, so it is what we send; the
/// confidentiality here comes from TLS and nothing else. Worth knowing before
/// deciding where the password is stored.
pub fn encode_password(password: &str) -> String {
    password.bytes().map(|b| format!("{b:02X}")).collect()
}

/// Bytes as uppercase hex, for the machine fingerprint.
pub fn encode_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// A reply that failed, with the server's own wording where it gave any.
#[derive(Debug, Clone, PartialEq)]
pub struct ShopError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ShopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "{} ({})", explain(&self.code), self.code)
        } else {
            write!(f, "{} ({}: {})", explain(&self.code), self.code, self.message)
        }
    }
}

impl std::error::Error for ShopError {}

/// The result codes the shop returns.
fn explain(code: &str) -> &'static str {
    match code {
        "1" => "ok",
        "2" => "the shop is in maintenance",
        "4" => "no such user",
        "5" => "this client version is no longer accepted by the shop",
        "6" => "wrong email or password",
        "10" => "this system name has been disabled",
        // Not in the reference client's table — it falls through to a generic
        // "operation cancelled". Observed when asking for an edition published
        // after the licence lapsed.
        "14" => "the shop would not grant this edition",
        "20" => "that chart is already assigned to this machine",
        "3d" => "no username given",
        "3e" => "invalid username",
        "3f" => "no password given",
        "3g" => "wrong password",
        "8h" => "the machine assigned to this system name has changed",
        "8j" => "this machine already has a system name",
        "8l" => "this machine is not known to the account yet",
        _ => "the shop refused the request",
    }
}

/// A parsed reply: the flat element values, plus the charts if any.
#[derive(Debug, Default, Clone)]
pub struct Reply {
    pub values: HashMap<String, String>,
    pub charts: Vec<Chart>,
    /// Machine names this account has registered.
    pub system_names: Vec<String>,
    pub grant: Option<DownloadGrant>,
}

impl Reply {
    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

/// Parse a reply, turning a non-`1` result into an error.
pub fn parse(xml: &str) -> Result<Reply, ShopError> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);

    let mut reply = Reply::default();
    let mut path: Vec<String> = Vec::new();
    let mut chart: Option<Chart> = None;
    let mut quantity: Option<Quantity> = None;
    let mut slot: Option<Slot> = None;
    let mut file: Option<FileGrant> = None;
    let mut text = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match name.as_str() {
                    "chart" => chart = Some(Chart::default()),
                    "quantity" => quantity = Some(Quantity::default()),
                    "slot" => slot = Some(Slot::default()),
                    "file" => file = Some(FileGrant::default()),
                    _ => {}
                }
                path.push(name);
                text.clear();
            }
            Ok(Event::Text(e)) => {
                text = e.unescape().unwrap_or_default().into_owned();
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let value = std::mem::take(&mut text);
                match name.as_str() {
                    "chart" => {
                        if let Some(c) = chart.take() {
                            reply.charts.push(c);
                        }
                    }
                    "quantity" => {
                        if let (Some(q), Some(c)) = (quantity.take(), chart.as_mut()) {
                            c.quantities.push(q);
                        }
                    }
                    "slot" => {
                        if let (Some(s), Some(q)) = (slot.take(), quantity.as_mut()) {
                            q.slots.push(s);
                        }
                    }
                    "file" => {
                        if let Some(f) = file.take() {
                            reply.grant.get_or_insert_with(Default::default).files.push(f);
                        }
                    }
                    "systemName" if chart.is_none() && slot.is_none() => {
                        reply.system_names.push(value.clone());
                        reply.values.insert(name.clone(), value);
                    }
                    _ => {
                        if let Some(f) = file.as_mut() {
                            set_file(f, &name, &value);
                        } else if let Some(s) = slot.as_mut() {
                            set_slot(s, &name, &value);
                        } else if let Some(q) = quantity.as_mut() {
                            if name == "quantityId" {
                                q.id = value.clone();
                            }
                        } else if let Some(c) = chart.as_mut() {
                            set_chart(c, &name, &value);
                        } else {
                            reply.values.insert(name.clone(), value);
                        }
                    }
                }
                path.pop();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ShopError {
                    code: "xml".into(),
                    message: e.to_string(),
                })
            }
            _ => {}
        }
        buf.clear();
    }

    // The result may be "code:message".
    let raw = reply.value("result").unwrap_or("").to_string();
    let (code, message) = match raw.split_once(':') {
        Some((c, m)) => (c.trim().to_string(), m.trim().to_string()),
        None => (raw.trim().to_string(), String::new()),
    };
    if code != "1" {
        return Err(ShopError { code, message });
    }
    Ok(reply)
}

fn set_chart(c: &mut Chart, name: &str, value: &str) {
    match name {
        "chartId" => c.id = value.into(),
        "chartName" => c.name = value.into(),
        "chartType" => c.chart_type = value.into(),
        "order" => c.order = value.into(),
        "purchase" => c.purchased = value.into(),
        "expiration" => c.expires = value.into(),
        "expired" => c.expired = value == "1",
        "edition" => c.edition = Edition::parse(value),
        "editionDate" => c.edition_date = value.into(),
        "maxSlots" => c.max_slots = value.parse().unwrap_or(0),
        "thumbLink" => c.thumbnail = value.into(),
        // Repeated: one element per link, not a nested list.
        "baseChartList" => c.base_chart_lists.push(value.into()),
        _ => {}
    }
}

fn set_slot(s: &mut Slot, name: &str, value: &str) {
    match name {
        "slotUuid" => s.uuid = value.into(),
        "assignedSystemName" => s.assigned_system = value.into(),
        "lastRequested" => s.last_requested = value.into(),
        _ => {}
    }
}

fn set_file(f: &mut FileGrant, name: &str, value: &str) {
    match name {
        "link" => f.url = value.into(),
        "size" => f.size = value.parse().unwrap_or(0),
        "sha256" => f.sha256 = value.to_ascii_lowercase(),
        "chartKeysLink" => f.keys_url = value.into(),
        "chartKeysSha256" => f.keys_sha256 = value.to_ascii_lowercase(),
        "editionTarget" => f.edition_target = value.into(),
        "editionResult" => f.edition_result = value.into(),
        _ => {}
    }
}

/// What to ask the shop for: which package, and which edition by name.
///
/// The edition is a string rather than an [`Edition`] because the shop's own
/// wording is the safest thing to echo back — a chart's `edition` arrives as
/// `2026/1-29` while a slot's `lastRequested` arrives as `1-20`, and
/// re-formatting either risks naming an edition that does not exist.
///
/// The expired case is the interesting one. A lapsed subscription still paid
/// for the editions published while it ran, so asking for the shop's *current*
/// edition asks for something the licence never covered and is refused. Ask
/// instead for the last edition this slot actually received, as a full base:
/// an update package is only valid against the exact edition it was built from.
pub fn choose_request(
    expired: bool,
    slot_last_requested: &str,
    installed: Option<Edition>,
    available: Edition,
) -> (DownloadTarget, String) {
    if expired {
        let entitled = Some(slot_last_requested.trim())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| installed.map(|e| e.to_string()));
        if let Some(version) = entitled {
            return (DownloadTarget::Base, version);
        }
    }
    (
        choose_download(installed, available),
        available.to_string(),
    )
}

/// Which package to ask for, given what is installed and what the shop has.
///
/// There is no "check for updates" call: `getlist` reports the shop's edition
/// for every chart and the client decides. An update package only carries the
/// cells that changed, so it is only valid against the exact edition it was
/// built from; anything else has to be a full base.
pub fn choose_download(installed: Option<Edition>, available: Edition) -> DownloadTarget {
    match installed {
        None => DownloadTarget::Base,
        Some(installed) => {
            if installed == available {
                DownloadTarget::Reinstall
            } else if available.major > installed.major || available.year != installed.year {
                // A new major edition is a new chart, not a patch on the old one.
                DownloadTarget::Base
            } else if available > installed {
                DownloadTarget::Update
            } else {
                DownloadTarget::UpToDate
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expired_licence_asks_for_the_edition_it_paid_for() {
        let shop_has = Edition::parse("2026/1-29");
        let on_disk = Some(Edition::parse("2025/1-20"));

        // Live subscription: the shop's current edition. A base, because the
        // year moved — only a minor bump within one edition can be patched.
        let (target, version) = choose_request(false, "1-20", on_disk, shop_has);
        assert_eq!(target, DownloadTarget::Base);
        assert_eq!(version, "2026/1-29");

        // Same edition, later update: that one is a patch.
        let (target, version) =
            choose_request(false, "1-20", on_disk, Edition::parse("2025/1-24"));
        assert_eq!(target, DownloadTarget::Update);
        assert_eq!(version, "2025/1-24");

        // Lapsed: ask for what the slot last received, as a whole base. Asking
        // for 2026/1-29 is asking for an edition published after the licence
        // ran out, and the shop answers 14.
        let (target, version) = choose_request(true, "1-20", on_disk, shop_has);
        assert_eq!(target, DownloadTarget::Base);
        assert_eq!(version, "1-20");

        // The shop's own string wins over ours, verbatim — including its
        // year-less form, which we must not "helpfully" reformat.
        let (_, version) = choose_request(true, "  2025/1-20 ", on_disk, shop_has);
        assert_eq!(version, "2025/1-20");

        // No record on the slot: fall back to what is installed here.
        let (target, version) = choose_request(true, "", on_disk, shop_has);
        assert_eq!(target, DownloadTarget::Base);
        assert_eq!(version, "2025/1-20");

        // Expired and nothing installed: there is no old edition to name, so
        // ask normally and let the shop refuse in its own words.
        let (_, version) = choose_request(true, "", None, shop_has);
        assert_eq!(version, "2026/1-29");
    }

    #[test]
    fn password_and_bytes_are_uppercase_hex() {
        assert_eq!(encode_password("aB1"), "614231");
        assert_eq!(encode_bytes(&[0x00, 0x0f, 0xff]), "000FFF");
    }

    #[test]
    fn form_encodes_the_awkward_characters() {
        let body = form(&[("username", "a b@c.dk"), ("password", "p+q&r")]);
        assert_eq!(body, "username=a+b%40c.dk&password=p%2Bq%26r");
    }

    #[test]
    fn a_refusal_becomes_an_error_with_the_servers_wording() {
        let e = parse("<response><result>6</result></response>").unwrap_err();
        assert_eq!(e.code, "6");
        assert!(e.to_string().contains("wrong email or password"));

        // The code may carry a message after a colon.
        let e = parse("<response><result>2:back at 09:00</result></response>").unwrap_err();
        assert_eq!(e.code, "2");
        assert_eq!(e.message, "back at 09:00");
    }

    #[test]
    fn login_returns_the_session_key() {
        let r = parse("<response><result>1</result><key>abc123</key></response>").unwrap();
        assert_eq!(r.value("key"), Some("abc123"));
    }

    #[test]
    fn a_chart_list_is_parsed_with_its_slots() {
        let xml = r#"
        <response>
          <result>1</result>
          <systemName>saloon-mac</systemName>
          <chart>
            <chartId>DK-1</chartId>
            <chartName>Denmark base</chartName>
            <chartType>oeuSENC</chartType>
            <order>ORD-9</order>
            <edition>2025/1-20</edition>
            <maxSlots>2</maxSlots>
            <expired>0</expired>
            <quantity>
              <quantityId>1</quantityId>
              <slot>
                <slotUuid>UU-1</slotUuid>
                <assignedSystemName>saloon-mac</assignedSystemName>
              </slot>
            </quantity>
          </chart>
        </response>"#;
        let r = parse(xml).unwrap();
        assert_eq!(r.system_names, vec!["saloon-mac"]);
        assert_eq!(r.charts.len(), 1);
        let c = &r.charts[0];
        assert_eq!(c.id, "DK-1");
        assert_eq!(c.name, "Denmark base");
        assert_eq!(c.max_slots, 2);
        assert!(!c.expired);
        assert_eq!(c.edition, Edition { year: 2025, major: 1, minor: 20 });
        assert_eq!(c.quantities.len(), 1);
        assert_eq!(c.quantities[0].slots[0].uuid, "UU-1");
        assert_eq!(c.quantities[0].slots[0].assigned_system, "saloon-mac");
    }

    #[test]
    fn a_download_grant_carries_both_urls_and_both_hashes() {
        let xml = r#"
        <response>
          <result>1</result>
          <file>
            <link>https://cdn.example/pkg.zip?sig=1</link>
            <size>1234</size>
            <sha256>AABB</sha256>
            <chartKeysLink>https://cdn.example/keys.xml</chartKeysLink>
            <chartKeysSha256>CCDD</chartKeysSha256>
            <editionResult>2025/1-20</editionResult>
          </file>
        </response>"#;
        let g = parse(xml).unwrap().grant.expect("a grant");
        assert_eq!(g.files.len(), 1);
        let f = &g.files[0];
        assert_eq!(f.url, "https://cdn.example/pkg.zip?sig=1");
        assert_eq!(f.size, 1234);
        // Hashes are compared lowercase, so they are stored that way.
        assert_eq!(f.sha256, "aabb");
        assert_eq!(f.keys_sha256, "ccdd");
        assert_eq!(f.edition_result, "2025/1-20");
    }

    #[test]
    fn update_versus_base_is_decided_by_the_edition() {
        let e = |y, ma, mi| Edition { year: y, major: ma, minor: mi };
        assert_eq!(choose_download(None, e(2025, 1, 20)), DownloadTarget::Base);
        assert_eq!(
            choose_download(Some(e(2025, 1, 20)), e(2025, 1, 20)),
            DownloadTarget::Reinstall
        );
        // Same major, later minor: a patch will do.
        assert_eq!(
            choose_download(Some(e(2025, 1, 18)), e(2025, 1, 20)),
            DownloadTarget::Update
        );
        // New major, or a new year: the whole chart set again.
        assert_eq!(
            choose_download(Some(e(2025, 1, 20)), e(2025, 2, 0)),
            DownloadTarget::Base
        );
        assert_eq!(
            choose_download(Some(e(2025, 1, 20)), e(2026, 1, 0)),
            DownloadTarget::Base
        );
        // The shop can be behind a machine that was updated elsewhere.
        assert_eq!(
            choose_download(Some(e(2025, 1, 20)), e(2025, 1, 19)),
            DownloadTarget::UpToDate
        );
    }

    #[test]
    fn the_client_version_names_the_platform_and_can_be_overridden() {
        let v = client_version();
        assert!(v.starts_with("w.") || v.starts_with("d.") || v.starts_with("l.") || v.starts_with("r."));
        assert!(v.len() > 2);
    }
}
