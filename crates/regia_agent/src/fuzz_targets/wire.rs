// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for wire data: the agent request and response text, voice-chat packets and the `regia_net` datagrams.

use super::{FuzzInput, FuzzTarget};
use crate::protocol::Response;

/// The wire targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    let mut all = vec![
        FuzzTarget {
            name: "agent.request",
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: agent_request_fixture,
                load: agent_request,
            },
        },
        FuzzTarget {
            name: "agent.response",
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: agent_response_fixture,
                load: agent_response,
            },
        },
    ];
    all.push(FuzzTarget {
        name: "voice.packet",
        format: None,
        format_version: u32::from(regia_audio::voice_chat::PACKET_VERSION),
        round_trip: true,
        input: FuzzInput::Bytes {
            fixture: voice_packet_fixture,
            load: voice_packet,
        },
    });
    all.extend(super::net::targets());
    all
}

/// A voice packet has no length word (the datagram frames it), so any cut after the 12-byte header is
/// a valid packet. The payload is shorter than the header so the harness's half-length truncation lands
/// inside the header and must be rejected. The codec is Opus because the mutator's growth can reach its
/// 400-byte payload cap.
fn voice_packet_fixture() -> Vec<u8> {
    use regia_audio::voice_chat::{VoiceCodecId, VoiceFlags, VoicePacket, VoiceQuality};
    let payload = [7u8; 8];
    let mut out = Vec::new();
    VoicePacket {
        flags: VoiceFlags::from_bits(0),
        sender: 1,
        channel: 2,
        seq: 3,
        timestamp: 480,
        quality: VoiceQuality::Standard,
        codec: VoiceCodecId::Opus,
        payload: &payload,
    }
    .write(&mut out);
    out
}

/// A voice-chat datagram (spec 59): parsed and written back.
fn voice_packet(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let packet = regia_audio::voice_chat::VoicePacket::parse(bytes).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    packet.write(&mut out);
    Ok(out)
}

fn agent_request_fixture() -> String {
    r#"{"jsonrpc":"2.0","id":7,"method":"play.step","params":{"frames":3,"note":"ok","list":[1,2.5,-3e2,true,null]}}"#
        .to_string()
}

/// The server's frame parse, then the dispatcher's pre-handler parameter check for the method.
fn agent_request(text: &str) -> Result<String, String> {
    let request = crate::server::parse_request(text).map_err(|r| {
        r.error
            .map_or_else(|| "invalid JSON-RPC".to_string(), |e| e.message)
    })?;
    crate::contract::check_params(crate::contract::contract(&request.method), &request.params)
        .map_err(|e| e.message)?;
    serde_json::to_string(&request).map_err(|e| e.to_string())
}

fn agent_response_fixture() -> String {
    r#"{"jsonrpc":"2.0","id":"a","error":{"code":-32602,"message":"bad","data":{"code":"F0002","hint":"h"}}}"#
        .to_string()
}

/// The client's response parse (`regia agent` and the MCP adapter read these).
fn agent_response(text: &str) -> Result<String, String> {
    let response: Response = serde_json::from_str(text).map_err(|e| e.to_string())?;
    serde_json::to_string(&response).map_err(|e| e.to_string())
}
