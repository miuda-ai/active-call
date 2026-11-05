use audio_codec::CodecType;
use rustrtc::MediaKind;
use rustrtc::sdp::SessionDescription;

#[derive(Clone)]
pub struct PeerMedia {
    pub rtp_addr: String,
    pub rtp_port: u16,
    pub rtcp_addr: String,
    pub rtcp_port: u16,
    pub rtcp_mux: bool,
    pub codecs: Vec<CodecType>,
    // From RFC 3551 6.  Payload Type Definitions
    // payload type values in the range 96-127 MAY be defined dynamically through a conference control protocol
    // From RFC 4566 5.14 Media Descriptions ("m=")
    // For dynamic payload type assignments the "a=rtpmap:" attribute (see
    // Section 6) SHOULD be used to map from an RTP payload type number
    // to a media encoding name that identifies the payload format.  The
    // "a=fmtp:"  attribute MAY be used to specify format parameters (see
    // Section 6).
    pub rtp_map: Vec<(u8, (CodecType, u32, u16))>,
}

pub fn parse_rtpmap(rtpmap: &str) -> Result<(u8, CodecType, u32, u16), anyhow::Error> {
    if let [payload_type_str, codec_spec] = rtpmap.split(' ').collect::<Vec<&str>>().as_slice() {
        // Parse payload type
        let payload_type = payload_type_str
            .parse::<u8>()
            .map_err(|e| anyhow::anyhow!("Failed to parse payload type: {}", e))?;
        let codec_parts: Vec<&str> = codec_spec.split('/').collect();

        if let [codec_name, clock_rate_str, channel_count @ ..] = codec_parts.as_slice() {
            let codec_type = CodecType::try_from(*codec_name)?;
            let clock_rate = clock_rate_str
                .parse::<u32>()
                .map_err(|e| anyhow::anyhow!("Failed to parse clock rate: {}", e))?;

            let channel_count = match channel_count {
                ["2"] => 2,
                _ => 1,
            };
            Ok((payload_type, codec_type, clock_rate, channel_count))
        } else {
            return Err(anyhow::anyhow!("Invalid codec specification in rtpmap"));
        }
    } else {
        Err(anyhow::anyhow!(
            "Invalid rtpmap format: missing space between payload type and encoding name"
        ))
    }
}

pub fn strip_ipv6_candidates(sdp: &str) -> String {
    sdp.lines()
        .filter(|line| !(line.starts_with("a=candidate:") && line.matches(':').count() >= 8))
        .collect::<Vec<&str>>()
        .join("\n")
        + "\n"
}

pub fn prefer_audio_codec(sdp: &SessionDescription) -> Option<CodecType> {
    let mut codecs = select_peer_media(sdp, "audio")?.codecs;
    codecs.sort_by(|a, b| a.cmp(b));
    codecs
        .iter()
        .filter(|codec| codec.is_audio())
        .last()
        .cloned()
}

/// Restricts the answer to codecs the offer has, in the offer's order, and
/// renumbers them to the offer's payload types. RFC 3264 6.1: the answerer
/// SHOULD use the offer's payload type for the same codec - we also send with
/// the answer's numbers, so otherwise our packets carry a dynamic payload type
/// the offerer never declared (opus 111 vs offered 114) and get dropped.
/// Dynamic payload types match by codec and clock rate, so e.g. our
/// telephone-event/48000 is dropped when only telephone-event/8000 is offered.
pub fn intersect_answer(offer: &SessionDescription, answer: &mut SessionDescription) {
    let Some(offer_media) = offer
        .media_sections
        .iter()
        .find(|m| m.kind == MediaKind::Audio)
    else {
        return;
    };
    let offer_formats = audio_formats(offer_media);

    for media in answer.media_sections.iter_mut() {
        if media.kind != MediaKind::Audio {
            continue;
        }
        let answer_formats = audio_formats(media);

        // answer payload type -> offer payload type, in the offer's order
        let mut renumber: Vec<(u8, u8)> = Vec::new();
        for (offer_pt, offer_codec, offer_clock) in &offer_formats {
            if renumber.iter().any(|(_, o)| o == offer_pt) {
                continue;
            }
            let matched = answer_formats.iter().find(|(pt, codec, clock)| {
                codec == offer_codec
                    // static payload types may come without rtpmap - no clock rate to compare
                    && (clock.is_none() || offer_clock.is_none() || clock == offer_clock)
                    && !renumber.iter().any(|(a, _)| a == pt)
            });
            if let Some((answer_pt, ..)) = matched {
                renumber.push((*answer_pt, *offer_pt));
            }
        }

        media.formats = renumber.iter().map(|(_, o)| o.to_string()).collect();

        // Keep rtpmap/fmtp/rtcp-fb of the kept payload types only, renumbered
        media.attributes.retain_mut(|attr| {
            if attr.key != "rtpmap" && attr.key != "fmtp" && attr.key != "rtcp-fb" {
                return true;
            }
            let Some(value) = &attr.value else {
                return false;
            };
            let (pt, rest) = value.split_once(' ').unwrap_or((value.as_str(), ""));
            let Ok(pt) = pt.parse::<u8>() else {
                return false;
            };
            let Some((_, offer_pt)) = renumber.iter().find(|(a, _)| *a == pt) else {
                return false;
            };
            attr.value = Some(format!("{offer_pt} {rest}").trim_end().to_string());
            true
        });
    }
}

/// (payload type, codec, clock rate) of an audio section's formats, in order.
/// Clock rate is None for static payload types listed without rtpmap.
fn audio_formats(media: &rustrtc::sdp::MediaSection) -> Vec<(u8, CodecType, Option<u32>)> {
    let rtpmaps: std::collections::HashMap<u8, (CodecType, u32)> = media
        .attributes
        .iter()
        .filter(|attr| attr.key == "rtpmap")
        .filter_map(|attr| parse_rtpmap(attr.value.as_deref()?).ok())
        .map(|(pt, codec, clock, _)| (pt, (codec, clock)))
        .collect();
    media
        .formats
        .iter()
        .filter_map(|fmt| {
            let pt = fmt.parse::<u8>().ok()?;
            match rtpmaps.get(&pt) {
                Some((codec, clock)) => Some((pt, *codec, Some(*clock))),
                None if pt < 96 => Some((pt, CodecType::try_from(pt).ok()?, None)),
                None => None,
            }
        })
        .collect()
}

pub fn select_peer_media(sdp: &SessionDescription, media_type: &str) -> Option<PeerMedia> {
    let mut peer_media = PeerMedia {
        rtp_addr: String::new(),
        rtcp_addr: String::new(),
        rtp_port: 0,
        rtcp_port: 0,
        rtcp_mux: false,
        codecs: Vec::new(),
        rtp_map: Vec::new(),
    };

    for media in sdp.media_sections.iter() {
        // Match media type (audio/video)
        let kind_str = match media.kind {
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
            _ => "unknown",
        };
        if kind_str == media_type {
            for attribute in media.attributes.iter() {
                if attribute.key == "rtpmap" {
                    if let Some(value) = &attribute.value {
                        if let Ok((pt, codec, clock, channels)) = parse_rtpmap(value) {
                            peer_media.rtp_map.push((pt, (codec, clock, channels)));
                        }
                    }
                }
                if attribute.key == "rtcp" {
                    attribute.value.as_ref().map(|v| {
                        // Parse the RTCP port from the attribute value
                        // Format is typically "port [IN IP4 address]"
                        let parts: Vec<&str> = v.split_whitespace().collect();
                        if !parts.is_empty() {
                            if let Ok(port) = parts[0].parse::<u16>() {
                                peer_media.rtcp_port = port;
                            }
                            if parts.len() >= 4 {
                                peer_media.rtcp_addr = parts[3].to_string();
                            }
                        }
                    });
                }
                if attribute.key == "rtcp-mux" {
                    peer_media.rtcp_mux = true;
                }
            }

            // Process formats
            media.formats.iter().for_each(|format| {
                if let Ok(digit) = format.parse::<u8>() {
                    // Dynamic payload type
                    if digit >= 96 && digit <= 127 {
                        if let Some((_, (codec, _, _))) = peer_media
                            .rtp_map
                            .iter()
                            .find(|(payload_type, _)| *payload_type == digit)
                        {
                            peer_media.codecs.push(*codec);
                        } else {
                            tracing::warn!("Unknown codec type: {}", digit);
                        }
                    } else {
                        if let Ok(codec) = CodecType::try_from(digit) {
                            peer_media.codecs.push(codec);
                        }
                    }
                }
            });

            peer_media.rtp_port = media.port;
            peer_media.rtcp_port = peer_media.rtp_port + 1;

            // Connection info from media level
            if let Some(conn) = &media.connection {
                // format typically: IN IP4 1.2.3.4
                let parts: Vec<&str> = conn.split_whitespace().collect();
                // Expect 3 parts: NetType AddrType Addr
                if parts.len() >= 3 {
                    // parts[2] is address
                    if peer_media.rtp_addr.is_empty() {
                        peer_media.rtp_addr = parts[2].to_string();
                    }
                    if peer_media.rtcp_addr.is_empty() {
                        peer_media.rtcp_addr = parts[2].to_string();
                    }
                }
            } else if let Some(conn) = &sdp.session.connection {
                // Fallback to session level
                let parts: Vec<&str> = conn.split_whitespace().collect();
                if parts.len() >= 3 {
                    if peer_media.rtp_addr.is_empty() {
                        peer_media.rtp_addr = parts[2].to_string();
                    }
                    if peer_media.rtcp_addr.is_empty() {
                        peer_media.rtcp_addr = parts[2].to_string();
                    }
                }
            }
            // Update rtcp_mux address after parsing everything
            if peer_media.rtcp_mux {
                peer_media.rtcp_addr = peer_media.rtp_addr.clone();
                peer_media.rtcp_port = peer_media.rtp_port;
            }
        }
    }
    Some(peer_media)
}

/// Detects if the SDP indicates a call on hold
/// According to RFC 3264, a call is on hold if:
/// - The media direction is "sendonly" or "inactive" (a=sendonly or a=inactive)
/// - The connection address is set to 0.0.0.0
pub fn detect_hold_state_from_sdp(sdp_str: &str) -> bool {
    // Try to parse the SDP
    let sdp = match SessionDescription::parse(rustrtc::sdp::SdpType::Offer, sdp_str) {
        Ok(sdp) => sdp,
        Err(_) => {
            // If parsing fails, try as Answer
            match SessionDescription::parse(rustrtc::sdp::SdpType::Answer, sdp_str) {
                Ok(sdp) => sdp,
                Err(_) => return false,
            }
        }
    };

    // Check for 0.0.0.0 connection address at session level
    if let Some(connection) = &sdp.session.connection {
        if connection.contains("0.0.0.0") {
            return true;
        }
    }

    // Check media sections for hold indicators
    for media in &sdp.media_sections {
        // Check connection address in media section
        if let Some(connection) = &media.connection {
            if connection.contains("0.0.0.0") {
                return true;
            }
        }

        // Check media direction (rustrtc stores this in the direction field)
        match media.direction {
            rustrtc::sdp::Direction::SendOnly | rustrtc::sdp::Direction::Inactive => {
                return true;
            }
            _ => {}
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use crate::media::negotiate::{prefer_audio_codec, select_peer_media};
    use audio_codec::CodecType;
    use rustrtc::sdp::SessionDescription;

    #[test]
    fn test_parse_freeswitch_sdp() {
        let offer = r#"v=0
o=FreeSWITCH 1745447592 1745447593 IN IP4 11.22.33.123
s=FreeSWITCH
c=IN IP4 11.22.33.123
t=0 0
m=audio 26328 RTP/AVP 0 101
a=rtpmap:0 PCMU/8000
a=rtpmap:101 telephone-event/8000
a=fmtp:101 0-16
a=ptime:20"#;
        let offer_sdp = SessionDescription::parse(rustrtc::sdp::SdpType::Offer, offer)
            .expect("Failed to parse SDP");
        let peer_media = select_peer_media(&offer_sdp, "audio").unwrap();
        assert_eq!(peer_media.rtp_port, 26328);
        assert_eq!(peer_media.rtcp_port, 26329);
        assert_eq!(peer_media.rtcp_addr, "11.22.33.123");
        assert_eq!(peer_media.rtp_addr, "11.22.33.123");
        assert_eq!(
            peer_media.codecs,
            vec![CodecType::PCMU, CodecType::TelephoneEvent]
        );

        let codec = prefer_audio_codec(&offer_sdp);
        assert_eq!(codec, Some(CodecType::PCMU));
    }

    #[test]
    fn test_answer_intersection() {
        use crate::media::negotiate::intersect_answer;
        // Offer with only PCMU (0) and telephone-event (101)
        let offer_str = r#"v=0
o=- 123 123 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9000 RTP/AVP 0 101
c=IN IP4 127.0.0.1
a=rtpmap:0 PCMU/8000
a=rtpmap:101 telephone-event/8000
"#;
        let offer = SessionDescription::parse(rustrtc::sdp::SdpType::Offer, offer_str).unwrap();

        // Answer with PCMA (8), PCMU (0), G722 (9)
        let answer_str = r#"v=0
o=- 456 456 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9000 RTP/AVP 8 0 9 101
c=IN IP4 127.0.0.1
a=rtpmap:8 PCMA/8000
a=rtpmap:0 PCMU/8000
a=rtpmap:9 G722/8000
a=rtpmap:101 telephone-event/8000
"#;
        let mut answer =
            SessionDescription::parse(rustrtc::sdp::SdpType::Answer, answer_str).unwrap();

        intersect_answer(&offer, &mut answer);

        // Verification
        let media = &answer.media_sections[0];

        // formats should only contain 0 and 101
        assert!(media.formats.contains(&"0".to_string()));
        assert!(media.formats.contains(&"101".to_string()));
        assert!(!media.formats.contains(&"8".to_string())); // PCMA should be removed
        assert!(!media.formats.contains(&"9".to_string())); // G722 should be removed

        // attributes check
        let rtpmap_values: Vec<&String> = media
            .attributes
            .iter()
            .filter(|a| a.key == "rtpmap")
            .filter_map(|a| a.value.as_ref())
            .collect();

        assert!(rtpmap_values.iter().any(|v| v.contains("PCMU/8000")));
        assert!(!rtpmap_values.iter().any(|v| v.contains("PCMA/8000")));
        assert!(!rtpmap_values.iter().any(|v| v.contains("G722/8000")));
    }

    #[test]
    fn test_answer_intersection_respects_offer_order() {
        use crate::media::negotiate::intersect_answer;
        // Offer with PCMA (8) preferred over PCMU (0)
        let offer_str = r#"v=0
o=- 123 123 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9000 RTP/AVP 8 0 101
c=IN IP4 127.0.0.1
a=rtpmap:8 PCMA/8000
a=rtpmap:0 PCMU/8000
a=rtpmap:101 telephone-event/8000
"#;
        let offer = SessionDescription::parse(rustrtc::sdp::SdpType::Offer, offer_str).unwrap();

        // Answer with PCMU (0) preferred over PCMA (8)
        let answer_str = r#"v=0
o=- 456 456 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9000 RTP/AVP 0 8 101
c=IN IP4 127.0.0.1
a=rtpmap:0 PCMU/8000
a=rtpmap:8 PCMA/8000
a=rtpmap:101 telephone-event/8000
"#;
        let mut answer =
            SessionDescription::parse(rustrtc::sdp::SdpType::Answer, answer_str).unwrap();

        intersect_answer(&offer, &mut answer);

        let media = &answer.media_sections[0];

        // The resulting formats should be in the offerer's preferred order: 8 then 0
        assert_eq!(
            media.formats,
            vec!["8".to_string(), "0".to_string(), "101".to_string()]
        );
    }

    /// Voximplant SBC offer: static payload types without rtpmap, opus as 114
    const VOX_OFFER: &str = "v=0\r
o=VIMS 234 1 IN IP4 185.164.148.135\r
s=VIMS\r
c=IN IP4 185.164.148.135\r
t=0 0\r
m=audio 15762 RTP/AVP 9 114 0 8 100 96 97 18 101\r
a=rtpmap:114 opus/48000/2\r
a=fmtp:114 minptime=10;useinbandfec=1;stereo=0;usedtx=1\r
a=rtcp-fb:114 transport-cc\r
a=rtpmap:100 ilbc/8000\r
a=rtpmap:96 AMR-WB/16000\r
a=rtpmap:101 telephone-event/8000\r
a=fmtp:101 0-15\r
a=sendrecv\r
";

    /// What we answer with before intersection: our numbering
    const OUR_ANSWER: &str = "v=0\r
o=- 1 1 IN IP4 31.129.43.67\r
s=-\r
c=IN IP4 31.129.43.67\r
t=0 0\r
m=audio 30816 RTP/AVP 9 111 8 0 101 110\r
a=rtpmap:9 G722/8000\r
a=rtpmap:111 opus/48000/2\r
a=fmtp:111 minptime=10;useinbandfec=1;stereo=1\r
a=rtpmap:8 PCMA/8000\r
a=rtpmap:0 PCMU/8000\r
a=rtpmap:101 telephone-event/8000\r
a=fmtp:101 0-16\r
a=rtpmap:110 telephone-event/48000\r
a=fmtp:110 0-16\r
a=sendrecv\r
";

    fn rtp_attrs(media: &rustrtc::sdp::MediaSection, key: &str) -> Vec<String> {
        media
            .attributes
            .iter()
            .filter(|a| a.key == key)
            .filter_map(|a| a.value.clone())
            .collect()
    }

    #[test]
    fn test_answer_intersection_uses_offer_payload_types() {
        use crate::media::negotiate::intersect_answer;
        let offer = SessionDescription::parse(rustrtc::sdp::SdpType::Offer, VOX_OFFER).unwrap();
        let mut answer =
            SessionDescription::parse(rustrtc::sdp::SdpType::Answer, OUR_ANSWER).unwrap();

        intersect_answer(&offer, &mut answer);

        let media = &answer.media_sections[0];
        // offer order, offer numbering; opus renumbered 111 -> 114
        assert_eq!(media.formats, vec!["9", "114", "0", "8", "101"]);
        let rtpmaps = rtp_attrs(media, "rtpmap");
        assert!(rtpmaps.contains(&"114 opus/48000/2".to_string()));
        assert!(rtpmaps.contains(&"101 telephone-event/8000".to_string()));
        assert!(!rtpmaps.iter().any(|v| v.starts_with("111 ")));
        // telephone-event/48000 wasn't offered
        assert!(!rtpmaps.iter().any(|v| v.contains("telephone-event/48000")));
        // our fmtp follows the renumbered payload type
        let fmtps = rtp_attrs(media, "fmtp");
        assert!(fmtps.contains(&"114 minptime=10;useinbandfec=1;stereo=1".to_string()));
        assert!(fmtps.contains(&"101 0-16".to_string()));
        assert!(!fmtps.iter().any(|v| v.starts_with("110 ")));
    }

    #[test]
    fn test_answer_intersection_matches_dynamic_by_clock_rate() {
        use crate::media::negotiate::intersect_answer;
        // offerer uses 96 for telephone-event/48000 and 97 for /8000
        let offer_str = "v=0\r
o=- 1 1 IN IP4 127.0.0.1\r
s=-\r
c=IN IP4 127.0.0.1\r
t=0 0\r
m=audio 9000 RTP/AVP 120 97 96\r
a=rtpmap:120 opus/48000/2\r
a=rtpmap:97 telephone-event/8000\r
a=rtpmap:96 telephone-event/48000\r
";
        let offer = SessionDescription::parse(rustrtc::sdp::SdpType::Offer, offer_str).unwrap();
        let mut answer =
            SessionDescription::parse(rustrtc::sdp::SdpType::Answer, OUR_ANSWER).unwrap();

        intersect_answer(&offer, &mut answer);

        let media = &answer.media_sections[0];
        assert_eq!(media.formats, vec!["120", "97", "96"]);
        let rtpmaps = rtp_attrs(media, "rtpmap");
        assert!(rtpmaps.contains(&"120 opus/48000/2".to_string()));
        assert!(rtpmaps.contains(&"97 telephone-event/8000".to_string()));
        assert!(rtpmaps.contains(&"96 telephone-event/48000".to_string()));
    }

    #[test]
    fn test_detect_hold_state() {
        use crate::media::negotiate::detect_hold_state_from_sdp;

        // Test sendonly (on hold)
        let sdp_sendonly = r#"v=0
o=- 654321 3 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9 RTP/AVP 0
c=IN IP4 127.0.0.1
a=sendonly
a=rtpmap:0 PCMU/8000
"#;
        assert!(
            detect_hold_state_from_sdp(sdp_sendonly),
            "Should detect sendonly as hold"
        );

        // Test inactive (on hold)
        let sdp_inactive = r#"v=0
o=- 654321 4 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9 RTP/AVP 0
c=IN IP4 127.0.0.1
a=inactive
a=rtpmap:0 PCMU/8000
"#;
        assert!(
            detect_hold_state_from_sdp(sdp_inactive),
            "Should detect inactive as hold"
        );

        // Test 0.0.0.0 connection (on hold)
        let sdp_zero_addr = r#"v=0
o=- 654321 5 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9 RTP/AVP 0
c=IN IP4 0.0.0.0
a=rtpmap:0 PCMU/8000
"#;
        assert!(
            detect_hold_state_from_sdp(sdp_zero_addr),
            "Should detect 0.0.0.0 as hold"
        );

        // Test sendrecv (active, not on hold)
        let sdp_active = r#"v=0
o=- 654321 2 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9 RTP/AVP 0
c=IN IP4 127.0.0.1
a=sendrecv
a=rtpmap:0 PCMU/8000
"#;
        assert!(
            !detect_hold_state_from_sdp(sdp_active),
            "Should detect sendrecv as active (not hold)"
        );

        // Test recvonly (not on hold from caller perspective)
        let sdp_recvonly = r#"v=0
o=- 654321 6 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9 RTP/AVP 0
c=IN IP4 127.0.0.1
a=recvonly
a=rtpmap:0 PCMU/8000
"#;
        assert!(
            !detect_hold_state_from_sdp(sdp_recvonly),
            "Should not detect recvonly as hold"
        );
    }
}
