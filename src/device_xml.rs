//! UPnP MediaRenderer device description + the three SCPDs. Static templates;
//! the only dynamic parts are uuid / friendlyName / manufacturer / model.
//! Every action listed in an SCPD is implemented in `actions.rs`; control
//! points read these to decide what they may call.
use crate::xml::xml_escape;

pub const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RCS: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
pub const CMS: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

pub struct DeviceIdentity<'a> {
    pub uuid: &'a str,
    pub friendly_name: &'a str,
    pub manufacturer: &'a str,
    pub model_name: &'a str,
}

pub fn device_description(d: &DeviceIdentity) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\
<specVersion><major>1</major><minor>0</minor></specVersion>\
<device>\
<deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>\
<friendlyName>{}</friendlyName>\
<manufacturer>{}</manufacturer>\
<manufacturerURL>https://github.com/coomteng/webos-cast-receiver</manufacturerURL>\
<modelName>{}</modelName>\
<modelDescription>DLNA renderer relaying to an LG webOS TV</modelDescription>\
<modelNumber>1.0</modelNumber>\
<UDN>uuid:{}</UDN>\
<dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>\
<serviceList>{}{}{}</serviceList>\
</device></root>",
        xml_escape(d.friendly_name),
        xml_escape(d.manufacturer),
        xml_escape(d.model_name),
        d.uuid,
        service(AVT, "AVTransport", "/dlna/avt"),
        service(RCS, "RenderingControl", "/dlna/rcs"),
        service(CMS, "ConnectionManager", "/dlna/cms"),
    )
}

fn service(kind: &str, id: &str, prefix: &str) -> String {
    format!(
        "<service><serviceType>{kind}</serviceType><serviceId>urn:upnp-org:serviceId:{id}</serviceId>\
<SCPDURL>{prefix}.xml</SCPDURL><controlURL>{prefix}/control</controlURL><eventSubURL>{prefix}/event</eventSubURL></service>"
    )
}

fn arg(name: &str, dir: &str, var: &str) -> String {
    format!("<argument><name>{name}</name><direction>{dir}</direction><relatedStateVariable>{var}</relatedStateVariable></argument>")
}
fn action(name: &str, args: &[String]) -> String {
    format!("<action><name>{name}</name><argumentList>{}</argumentList></action>", args.concat())
}
fn sv(name: &str, kind: &str, events: bool, allowed: &[&str], def: Option<&str>) -> String {
    let mut s = format!(
        "<stateVariable sendEvents=\"{}\"><name>{name}</name><dataType>{kind}</dataType>",
        if events { "yes" } else { "no" }
    );
    if let Some(d) = def {
        s.push_str(&format!("<defaultValue>{d}</defaultValue>"));
    }
    if !allowed.is_empty() {
        s.push_str("<allowedValueList>");
        for v in allowed {
            s.push_str(&format!("<allowedValue>{v}</allowedValue>"));
        }
        s.push_str("</allowedValueList>");
    }
    s + "</stateVariable>"
}
fn scpd(actions: &[String], vars: &[String]) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion>\
<actionList>{}</actionList><serviceStateTable>{}</serviceStateTable></scpd>",
        actions.concat(),
        vars.concat()
    )
}

pub fn avt_scpd() -> String {
    let id = arg("InstanceID", "in", "A_ARG_TYPE_InstanceID");
    let a = |n: &str, rest: &[String]| {
        let mut v = vec![id.clone()];
        v.extend_from_slice(rest);
        action(n, &v)
    };
    scpd(
        &[
            a("SetAVTransportURI", &[arg("CurrentURI", "in", "AVTransportURI"), arg("CurrentURIMetaData", "in", "AVTransportURIMetaData")]),
            a("SetNextAVTransportURI", &[arg("NextURI", "in", "NextAVTransportURI"), arg("NextURIMetaData", "in", "NextAVTransportURIMetaData")]),
            a("GetMediaInfo", &[
                arg("NrTracks", "out", "NumberOfTracks"), arg("MediaDuration", "out", "CurrentMediaDuration"),
                arg("CurrentURI", "out", "AVTransportURI"), arg("CurrentURIMetaData", "out", "AVTransportURIMetaData"),
                arg("NextURI", "out", "NextAVTransportURI"), arg("NextURIMetaData", "out", "NextAVTransportURIMetaData"),
                arg("PlayMedium", "out", "PlaybackStorageMedium"), arg("RecordMedium", "out", "RecordStorageMedium"),
                arg("WriteStatus", "out", "RecordMediumWriteStatus"),
            ]),
            a("GetTransportInfo", &[
                arg("CurrentTransportState", "out", "TransportState"), arg("CurrentTransportStatus", "out", "TransportStatus"),
                arg("CurrentSpeed", "out", "TransportPlaySpeed"),
            ]),
            a("GetPositionInfo", &[
                arg("Track", "out", "CurrentTrack"), arg("TrackDuration", "out", "CurrentTrackDuration"),
                arg("TrackMetaData", "out", "CurrentTrackMetaData"), arg("TrackURI", "out", "CurrentTrackURI"),
                arg("RelTime", "out", "RelativeTimePosition"), arg("AbsTime", "out", "AbsoluteTimePosition"),
                arg("RelCount", "out", "RelativeCounterPosition"), arg("AbsCount", "out", "AbsoluteCounterPosition"),
            ]),
            a("GetDeviceCapabilities", &[
                arg("PlayMedia", "out", "PossiblePlaybackStorageMedia"), arg("RecMedia", "out", "PossibleRecordStorageMedia"),
                arg("RecQualityModes", "out", "PossibleRecordQualityModes"),
            ]),
            a("GetTransportSettings", &[arg("PlayMode", "out", "CurrentPlayMode"), arg("RecQualityMode", "out", "CurrentRecordQualityMode")]),
            a("Stop", &[]),
            a("Play", &[arg("Speed", "in", "TransportPlaySpeed")]),
            a("Pause", &[]),
            a("Seek", &[arg("Unit", "in", "A_ARG_TYPE_SeekMode"), arg("Target", "in", "A_ARG_TYPE_SeekTarget")]),
            a("Next", &[]),
            a("Previous", &[]),
            a("SetPlayMode", &[arg("NewPlayMode", "in", "CurrentPlayMode")]),
            a("GetCurrentTransportActions", &[arg("Actions", "out", "CurrentTransportActions")]),
        ],
        &[
            sv("TransportState", "string", false, &["STOPPED", "PLAYING", "TRANSITIONING", "PAUSED_PLAYBACK", "NO_MEDIA_PRESENT"], None),
            sv("TransportStatus", "string", false, &["OK", "ERROR_OCCURRED"], None),
            sv("PlaybackStorageMedium", "string", false, &["NETWORK", "NONE"], None),
            sv("RecordStorageMedium", "string", false, &["NOT_IMPLEMENTED"], None),
            sv("PossiblePlaybackStorageMedia", "string", false, &[], None),
            sv("PossibleRecordStorageMedia", "string", false, &[], None),
            sv("CurrentPlayMode", "string", false, &["NORMAL"], Some("NORMAL")),
            sv("TransportPlaySpeed", "string", false, &["1"], None),
            sv("RecordMediumWriteStatus", "string", false, &["NOT_IMPLEMENTED"], None),
            sv("CurrentRecordQualityMode", "string", false, &["NOT_IMPLEMENTED"], None),
            sv("PossibleRecordQualityModes", "string", false, &[], None),
            sv("NumberOfTracks", "ui4", false, &[], None),
            sv("CurrentTrack", "ui4", false, &[], None),
            sv("CurrentTrackDuration", "string", false, &[], None),
            sv("CurrentMediaDuration", "string", false, &[], None),
            sv("CurrentTrackMetaData", "string", false, &[], None),
            sv("CurrentTrackURI", "string", false, &[], None),
            sv("AVTransportURI", "string", false, &[], None),
            sv("AVTransportURIMetaData", "string", false, &[], None),
            sv("NextAVTransportURI", "string", false, &[], None),
            sv("NextAVTransportURIMetaData", "string", false, &[], None),
            sv("RelativeTimePosition", "string", false, &[], None),
            sv("AbsoluteTimePosition", "string", false, &[], None),
            sv("RelativeCounterPosition", "i4", false, &[], None),
            sv("AbsoluteCounterPosition", "i4", false, &[], None),
            sv("CurrentTransportActions", "string", false, &[], None),
            sv("LastChange", "string", true, &[], None),
            sv("A_ARG_TYPE_SeekMode", "string", false, &["REL_TIME", "ABS_TIME", "TRACK_NR"], None),
            sv("A_ARG_TYPE_SeekTarget", "string", false, &[], None),
            sv("A_ARG_TYPE_InstanceID", "ui4", false, &[], None),
        ],
    )
}

pub fn rcs_scpd() -> String {
    let id = arg("InstanceID", "in", "A_ARG_TYPE_InstanceID");
    let ch = arg("Channel", "in", "A_ARG_TYPE_Channel");
    scpd(
        &[
            action("ListPresets", &[id.clone(), arg("CurrentPresetNameList", "out", "PresetNameList")]),
            action("SelectPreset", &[id.clone(), arg("PresetName", "in", "A_ARG_TYPE_PresetName")]),
            action("GetMute", &[id.clone(), ch.clone(), arg("CurrentMute", "out", "Mute")]),
            action("SetMute", &[id.clone(), ch.clone(), arg("DesiredMute", "in", "Mute")]),
            action("GetVolume", &[id.clone(), ch.clone(), arg("CurrentVolume", "out", "Volume")]),
            action("SetVolume", &[id, ch, arg("DesiredVolume", "in", "Volume")]),
        ],
        &[
            sv("PresetNameList", "string", false, &[], None),
            sv("LastChange", "string", true, &[], None),
            sv("Mute", "boolean", false, &[], None),
            "<stateVariable sendEvents=\"no\"><name>Volume</name><dataType>ui2</dataType>\
<allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange></stateVariable>".to_string(),
            sv("A_ARG_TYPE_Channel", "string", false, &["Master"], None),
            sv("A_ARG_TYPE_InstanceID", "ui4", false, &[], None),
            sv("A_ARG_TYPE_PresetName", "string", false, &["FactoryDefaults"], None),
        ],
    )
}

pub fn cms_scpd() -> String {
    scpd(
        &[
            action("GetProtocolInfo", &[arg("Source", "out", "SourceProtocolInfo"), arg("Sink", "out", "SinkProtocolInfo")]),
            action("GetCurrentConnectionIDs", &[arg("ConnectionIDs", "out", "CurrentConnectionIDs")]),
            action("GetCurrentConnectionInfo", &[
                arg("ConnectionID", "in", "A_ARG_TYPE_ConnectionID"),
                arg("RcsID", "out", "A_ARG_TYPE_RcsID"), arg("AVTransportID", "out", "A_ARG_TYPE_AVTransportID"),
                arg("ProtocolInfo", "out", "A_ARG_TYPE_ProtocolInfo"), arg("PeerConnectionManager", "out", "A_ARG_TYPE_ConnectionManager"),
                arg("PeerConnectionID", "out", "A_ARG_TYPE_ConnectionID"), arg("Direction", "out", "A_ARG_TYPE_Direction"),
                arg("Status", "out", "A_ARG_TYPE_ConnectionStatus"),
            ]),
        ],
        &[
            sv("SourceProtocolInfo", "string", true, &[], None),
            sv("SinkProtocolInfo", "string", true, &[], None),
            sv("CurrentConnectionIDs", "string", true, &[], None),
            sv("A_ARG_TYPE_ConnectionStatus", "string", false, &["OK", "ContentFormatMismatch", "InsufficientBandwidth", "UnreliableChannel", "Unknown"], None),
            sv("A_ARG_TYPE_ConnectionManager", "string", false, &[], None),
            sv("A_ARG_TYPE_Direction", "string", false, &["Input", "Output"], None),
            sv("A_ARG_TYPE_ProtocolInfo", "string", false, &[], None),
            sv("A_ARG_TYPE_ConnectionID", "i4", false, &[], None),
            sv("A_ARG_TYPE_AVTransportID", "i4", false, &[], None),
            sv("A_ARG_TYPE_RcsID", "i4", false, &[], None),
        ],
    )
}

/// What we claim to be able to play. Broad on purpose: the TV's `<video>`
/// decides in the end, and a narrow list makes phones hide the device.
pub const SINK_PROTOCOL_INFO: &str = "http-get:*:video/mp4:*,http-get:*:video/x-m4v:*,http-get:*:video/3gpp:*,\
http-get:*:video/quicktime:*,http-get:*:video/x-matroska:*,http-get:*:video/webm:*,\
http-get:*:video/mpeg:*,http-get:*:video/mp2t:*,http-get:*:video/x-flv:*,\
http-get:*:application/vnd.apple.mpegurl:*,http-get:*:application/x-mpegurl:*,\
http-get:*:audio/mpeg:*,http-get:*:audio/mp4:*,http-get:*:audio/x-m4a:*,\
http-get:*:audio/aac:*,http-get:*:audio/wav:*,http-get:*:audio/flac:*,\
http-get:*:image/jpeg:*,http-get:*:image/png:*,http-get:*:*:*";

/// Action names listed in an SCPD (used by tests to cross-check the dispatcher).
pub fn listed_actions(scpd: &str) -> Vec<String> {
    regex::Regex::new(r"<action><name>([A-Za-z]+)</name>")
        .unwrap()
        .captures_iter(scpd)
        .map(|c| c[1].to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn description_shape() {
        let x = device_description(&DeviceIdentity { uuid: "u1", friendly_name: "客厅 & 电视", manufacturer: "M", model_name: "N" });
        assert!(x.contains("<deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>"));
        assert!(x.contains("<friendlyName>客厅 &amp; 电视</friendlyName>"));
        assert!(x.contains("<UDN>uuid:u1</UDN>"));
        assert_eq!(x.matches("<service>").count(), 3);
        assert!(x.contains("<controlURL>/dlna/avt/control</controlURL>"));
        assert!(x.contains("<eventSubURL>/dlna/avt/event</eventSubURL>"));
    }
}
