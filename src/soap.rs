//! SOAP request parsing, response envelopes, UPnP errors, time formats.
use crate::xml::{tag_value, xml_escape, xml_unescape};
use regex::Regex;
use std::sync::OnceLock;

/// UPnP error codes: 401 invalid action, 402 invalid args, 501 action failed,
/// 701 transition not available, 710 seek mode not supported, 711 illegal
/// seek target, 712 play mode not supported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpnpError {
    pub code: u16,
    pub message: String,
}

impl UpnpError {
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for UpnpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UPnP {} {}", self.code, self.message)
    }
}

/// `"urn:...:service:AVTransport:1#Play"` -> `Play`. Header may be quoted.
pub fn action_from_soapaction(header: Option<&str>) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"#([A-Za-z0-9_]+)"?\s*$"#).unwrap());
    re.captures(header?).map(|c| c[1].to_string())
}

pub fn soap_arg(body: &str, name: &str) -> Option<String> {
    tag_value(body, name).map(|v| xml_unescape(&v))
}

pub fn soap_response(service: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut inner = String::new();
    for (k, v) in args {
        inner.push_str(&format!("<{k}>{}</{k}>", xml_escape(v)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><u:{action}Response xmlns:u=\"{service}\">{inner}</u:{action}Response></s:Body></s:Envelope>"
    )
}

pub fn soap_fault(code: u16, description: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
<detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
<errorCode>{code}</errorCode><errorDescription>{}</errorDescription>\
</UPnPError></detail></s:Fault></s:Body></s:Envelope>",
        xml_escape(description)
    )
}

/// `"0:01:23"`, `"00:01:23.500"`, `"1:23"` or plain seconds -> seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    static PLAIN: OnceLock<Regex> = OnceLock::new();
    static HMS: OnceLock<Regex> = OnceLock::new();
    let t = s.trim();
    if PLAIN.get_or_init(|| Regex::new(r"^\d+(\.\d+)?$").unwrap()).is_match(t) {
        return t.parse().ok();
    }
    let re = HMS.get_or_init(|| Regex::new(r"^(?:(\d+):)?(\d{1,2}):(\d{1,2})(?:\.(\d+))?$").unwrap());
    let c = re.captures(t)?;
    let h: f64 = c.get(1).map_or(0.0, |m| m.as_str().parse().unwrap_or(0.0));
    let m: f64 = c[2].parse().ok()?;
    let sec: f64 = c[3].parse().ok()?;
    let frac: f64 = c.get(4).map_or(0.0, |f| format!("0.{}", f.as_str()).parse().unwrap_or(0.0));
    Some(h * 3600.0 + m * 60.0 + sec + frac)
}

/// seconds -> `H:MM:SS` (no fraction; most control points choke on it).
pub fn format_time(sec: f64) -> String {
    let sec = if sec.is_finite() && sec >= 0.0 { sec } else { 0.0 };
    let s = sec.floor() as u64;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soapaction_quoted_unquoted() {
        assert_eq!(action_from_soapaction(Some("\"urn:schemas-upnp-org:service:AVTransport:1#Play\"")).as_deref(), Some("Play"));
        assert_eq!(action_from_soapaction(Some("urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI")).as_deref(), Some("SetAVTransportURI"));
        assert_eq!(action_from_soapaction(None), None);
    }

    #[test]
    fn soap_arg_extracts_and_unescapes() {
        let body = "<s:Envelope><s:Body><u:SetAVTransportURI xmlns:u=\"x\"><InstanceID>0</InstanceID>\
<CurrentURI>http://a/b?x=1&amp;y=2</CurrentURI><CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>\
</u:SetAVTransportURI></s:Body></s:Envelope>";
        assert_eq!(soap_arg(body, "CurrentURI").as_deref(), Some("http://a/b?x=1&y=2"));
        assert_eq!(soap_arg(body, "CurrentURIMetaData").as_deref(), Some("<DIDL-Lite></DIDL-Lite>"));
        assert_eq!(soap_arg(body, "Nope"), None);
    }

    #[test]
    fn response_and_fault_shapes() {
        let r = soap_response("urn:x", "GetVolume", &[("CurrentVolume", "42".into())]);
        assert!(r.contains("<u:GetVolumeResponse xmlns:u=\"urn:x\"><CurrentVolume>42</CurrentVolume></u:GetVolumeResponse>"));
        let f = soap_fault(701, "Transition not available");
        assert!(f.contains("<errorCode>701</errorCode>"));
        assert!(f.contains("<faultstring>UPnPError</faultstring>"));
    }

    #[test]
    fn time_parse_format() {
        assert_eq!(parse_time("0:01:23"), Some(83.0));
        assert_eq!(parse_time("00:00:05.500"), Some(5.5));
        assert_eq!(parse_time("1:02"), Some(62.0));
        assert_eq!(parse_time("12"), Some(12.0));
        assert_eq!(parse_time("abc"), None);
        assert_eq!(format_time(83.0), "0:01:23");
        assert_eq!(format_time(3725.9), "1:02:05");
        assert_eq!(format_time(f64::NAN), "0:00:00");
    }
}
