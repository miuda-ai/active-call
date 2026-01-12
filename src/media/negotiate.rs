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
}
