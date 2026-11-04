//! SOAP action dispatch for the three services. Each handler takes the
//! renderer and the parsed body, returns out-arguments or a `UpnpError`.
use crate::device_xml::SINK_PROTOCOL_INFO;
use crate::renderer::Renderer;
use crate::soap::{format_time, parse_time, soap_arg, UpnpError};

pub type Out = Vec<(&'static str, String)>;

pub const AVT_ACTIONS: &[&str] = &[
    "SetAVTransportURI", "SetNextAVTransportURI", "GetMediaInfo", "GetTransportInfo", "GetPositionInfo",
    "GetDeviceCapabilities", "GetTransportSettings", "GetCurrentTransportActions", "Stop", "Play", "Pause",
    "Seek", "Next", "Previous", "SetPlayMode",
];
pub const RCS_ACTIONS: &[&str] = &["ListPresets", "SelectPreset", "GetMute", "SetMute", "GetVolume", "SetVolume"];
pub const CMS_ACTIONS: &[&str] = &["GetProtocolInfo", "GetCurrentConnectionIDs", "GetCurrentConnectionInfo"];

fn s(v: impl ToString) -> String {
    v.to_string()
}

/// `None` = unknown action (UPnP 401 at the HTTP layer).
pub fn avt(r: &mut Renderer, action: &str, body: &str) -> Option<Result<Out, UpnpError>> {
    let arg = |n: &str| soap_arg(body, n).unwrap_or_default();
    Some(match action {
        "SetAVTransportURI" => r.set_uri(&arg("CurrentURI"), &arg("CurrentURIMetaData")).map(|_| vec![]),
        "SetNextAVTransportURI" => {
            r.set_next_uri(&arg("NextURI"), &arg("NextURIMetaData"));
            Ok(vec![])
        }
        "GetMediaInfo" => Ok(vec![
            ("NrTracks", s(if r.uri.is_empty() { 0 } else { 1 })),
            ("MediaDuration", format_time(r.duration)),
            ("CurrentURI", r.uri.clone()),
            ("CurrentURIMetaData", r.metadata.clone()),
            ("NextURI", r.next_uri.clone()),
            ("NextURIMetaData", r.next_metadata.clone()),
            ("PlayMedium", s(if r.uri.is_empty() { "NONE" } else { "NETWORK" })),
            ("RecordMedium", s("NOT_IMPLEMENTED")),
            ("WriteStatus", s("NOT_IMPLEMENTED")),
        ]),
        "GetTransportInfo" => Ok(vec![
            ("CurrentTransportState", s(r.state.as_str())),
            ("CurrentTransportStatus", s(r.status)),
            ("CurrentSpeed", s("1")),
        ]),
        "GetPositionInfo" => {
            let pos = format_time(r.position());
            Ok(vec![
                ("Track", s(if r.uri.is_empty() { 0 } else { 1 })),
                ("TrackDuration", format_time(r.duration)),
                ("TrackMetaData", r.metadata.clone()),
                ("TrackURI", r.uri.clone()),
                ("RelTime", pos.clone()),
                ("AbsTime", pos),
                ("RelCount", s(2147483647)),
                ("AbsCount", s(2147483647)),
            ])
        }
        "GetDeviceCapabilities" => Ok(vec![
            ("PlayMedia", s("NETWORK")),
            ("RecMedia", s("NOT_IMPLEMENTED")),
            ("RecQualityModes", s("NOT_IMPLEMENTED")),
        ]),
        "GetTransportSettings" => Ok(vec![("PlayMode", s("NORMAL")), ("RecQualityMode", s("NOT_IMPLEMENTED"))]),
        "GetCurrentTransportActions" => Ok(vec![("Actions", s(r.current_actions()))]),
        "Stop" => {
            r.stop();
            Ok(vec![])
        }
        "Play" => r.play().map(|_| vec![]),
        "Pause" => r.pause().map(|_| vec![]),
        "Seek" => {
            let unit = soap_arg(body, "Unit").filter(|u| !u.is_empty()).unwrap_or_else(|| "REL_TIME".into());
            if unit == "TRACK_NR" {
                return Some(Ok(vec![])); // single track: nothing to do
            }
            if unit != "REL_TIME" && unit != "ABS_TIME" {
                return Some(Err(UpnpError::new(710, "seek mode not supported")));
            }
            match parse_time(&arg("Target")) {
                Some(sec) => r.seek(sec).map(|_| vec![]),
                None => Err(UpnpError::new(711, "illegal seek target")),
            }
        }
        // No playlist: Next means "finish this clip". Phones that queued a
        // NextURI get the ended signal and push the next one themselves.
        "Next" => {
            r.stop();
            Ok(vec![])
        }
        "Previous" => r.seek(0.0).map(|_| vec![]),
        "SetPlayMode" => {
            let mode = arg("NewPlayMode");
            if !mode.is_empty() && mode != "NORMAL" {
                Err(UpnpError::new(712, "play mode not supported"))
            } else {
                Ok(vec![])
            }
        }
        _ => return None,
    })
}

pub fn rcs(r: &mut Renderer, action: &str, body: &str) -> Option<Result<Out, UpnpError>> {
    Some(match action {
        "ListPresets" => Ok(vec![("CurrentPresetNameList", s("FactoryDefaults"))]),
        "SelectPreset" => Ok(vec![]),
        "GetMute" => Ok(vec![("CurrentMute", s(if r.mute { 1 } else { 0 }))]),
        "SetMute" => {
            let v = soap_arg(body, "DesiredMute").unwrap_or_default();
            r.set_mute(v == "1" || v == "true" || v == "True");
            Ok(vec![])
        }
        "GetVolume" => Ok(vec![("CurrentVolume", s(r.volume))]),
        "SetVolume" => r.set_volume(&soap_arg(body, "DesiredVolume").unwrap_or_default()).map(|_| vec![]),
        _ => return None,
    })
}

pub fn cms(action: &str) -> Option<Result<Out, UpnpError>> {
    Some(Ok(match action {
        "GetProtocolInfo" => vec![("Source", s("")), ("Sink", s(SINK_PROTOCOL_INFO))],
        "GetCurrentConnectionIDs" => vec![("ConnectionIDs", s("0"))],
        "GetCurrentConnectionInfo" => vec![
            ("RcsID", s(0)),
            ("AVTransportID", s(0)),
            ("ProtocolInfo", s("")),
            ("PeerConnectionManager", s("")),
            ("PeerConnectionID", s(-1)),
            ("Direction", s("Input")),
            ("Status", s("OK")),
        ],
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_xml::{avt_scpd, cms_scpd, listed_actions, rcs_scpd};
    use crate::renderer::tests::make;
    use crate::renderer::TvReport;

    fn get<'a>(out: &'a Out, k: &str) -> &'a str {
        out.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str()).unwrap()
    }

    #[test]
    fn scpds_match_dispatcher() {
        let mut h = make(true);
        for (scpd, list) in [(avt_scpd(), AVT_ACTIONS), (rcs_scpd(), RCS_ACTIONS), (cms_scpd(), CMS_ACTIONS)] {
            let listed = listed_actions(&scpd);
            for a in &listed {
                assert!(list.contains(&a.as_str()), "{a} listed but not in table");
            }
            for a in list {
                assert!(listed.contains(&a.to_string()), "{a} in table but not listed");
            }
        }
        // every table entry actually dispatches (Get* variants are side-effect free)
        for a in AVT_ACTIONS {
            assert!(avt(&mut h.r, a, "").is_some(), "{a}");
        }
        for a in RCS_ACTIONS {
            assert!(rcs(&mut h.r, a, "").is_some(), "{a}");
        }
        for a in CMS_ACTIONS {
            assert!(cms(a).is_some(), "{a}");
        }
        assert!(avt(&mut h.r, "Bogus", "").is_none());
    }

    #[test]
    fn position_transport_seek() {
        let mut h = make(true);
        avt(&mut h.r, "SetAVTransportURI", "<CurrentURI>http://x/a.mp4</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>").unwrap().unwrap();
        h.r.on_tv_report(&TvReport { load_id: Some(1), state: Some("playing".into()), position: Some(0.0), duration: Some(83.0), ..Default::default() });
        h.tick(5000.0);
        let pi = avt(&mut h.r, "GetPositionInfo", "").unwrap().unwrap();
        assert_eq!(get(&pi, "TrackDuration"), "0:01:23");
        assert_eq!(get(&pi, "RelTime"), "0:00:05");
        assert_eq!(get(&pi, "TrackURI"), "http://x/a.mp4");
        let ti = avt(&mut h.r, "GetTransportInfo", "").unwrap().unwrap();
        assert_eq!(get(&ti, "CurrentTransportState"), "PLAYING");
        avt(&mut h.r, "Seek", "<Unit>REL_TIME</Unit><Target>0:00:40</Target>").unwrap().unwrap();
        assert!((h.r.position() - 40.0).abs() < 0.01);
        assert_eq!(avt(&mut h.r, "Seek", "<Unit>REL_TIME</Unit><Target>zz</Target>").unwrap().unwrap_err().code, 711);
        assert_eq!(avt(&mut h.r, "Seek", "<Unit>X</Unit><Target>1</Target>").unwrap().unwrap_err().code, 710);
        assert_eq!(get(&avt(&mut h.r, "GetMediaInfo", "").unwrap().unwrap(), "NrTracks"), "1");
        assert_eq!(get(&avt(&mut h.r, "GetCurrentTransportActions", "").unwrap().unwrap(), "Actions"), "Pause,Stop,Seek,Next");
    }
}
