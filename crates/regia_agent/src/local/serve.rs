// SPDX-License-Identifier: MPL-2.0
//! The serve side of the local transport (spec 23 § 2): the bearer handshake and the
//! per-connection JSON-RPC loop. Only platforms with a local transport run this (Linux, macOS,
//! Windows); Android has no local connections to serve.

use crossbeam_channel::{Receiver, Sender, bounded};
use serde::Deserialize;

use crate::local::{
    HANDSHAKE_ACCEPTED, HANDSHAKE_REFUSED, MAX_FRAME_BYTES, MAX_HANDSHAKE_BYTES, read_frame,
    read_frame_before, write_frame,
};
use crate::platform::LocalStream;
use crate::protocol::Response;
use crate::render_out::RenderOutBoard;
use crate::server::{self, Inbound};

/// The client's bearer-token handshake: the first frame of every local connection must be
/// exactly this object, with the token the endpoint's `token` file holds, in at most
/// `MAX_HANDSHAKE_BYTES`. The server answers [`HANDSHAKE_ACCEPTED`] or, the way the WebSocket
/// handshake answers 401, [`HANDSHAKE_REFUSED`] and closes (ADR-5878).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BearerHandshake {
    bearer: String,
}

/// Serves one local connection: the bearer-token handshake, then JSON-RPC frames until the
/// peer disconnects or a violation ends the connection. Mirrors `server::serve_connection`:
/// requests queue for the world with the request's own timeout, notifications are bounded by
/// `server::MAX_QUEUED_NOTIFICATIONS`, and a request the world does not take in time is
/// answered F0006 and marked abandoned so the world skips it later. A queued
/// `render.out_cancel` or `agent.job_cancel` wakes an app thread waiting on a stalled
/// render-out output (ADR-2396), as it does over the WebSocket.
///
/// The handshake must arrive whole within `server::HANDSHAKE_TIMEOUT` in total (each read gets
/// only the time left), so a silent or trickling peer cannot hold a connection slot.
pub(crate) fn serve_connection(
    mut stream: LocalStream,
    token: &str,
    inbox: &Sender<Inbound>,
    board: Option<&RenderOutBoard>,
) {
    let deadline = std::time::Instant::now() + server::HANDSHAKE_TIMEOUT;
    let handshake = match read_frame_before(&mut stream, MAX_HANDSHAKE_BYTES, deadline) {
        Ok(Some(frame)) => Some(frame),
        // A clean close before any frame (a liveness probe): nothing to answer.
        Ok(None) => return,
        Err(_) => None,
    };
    let accepted = handshake.as_deref().is_some_and(|frame| {
        serde_json::from_str::<BearerHandshake>(frame)
            .is_ok_and(|hello| server::constant_time_eq(hello.bearer.as_bytes(), token.as_bytes()))
    });
    if !accepted {
        tracing::warn!(target: "regia_agent", "rejected unauthorized local connection");
        // Best effort: the peer may be gone already.
        let _ = write_frame(&mut stream, HANDSHAKE_REFUSED.as_bytes());
        return;
    }
    // An authenticated local client may idle as long as it likes, as on the WebSocket path.
    if stream.set_read_timeout(None).is_err()
        || write_frame(&mut stream, HANDSHAKE_ACCEPTED.as_bytes()).is_err()
    {
        return;
    }
    let mut connection = server::ConnectionGuard::new();
    let mut queued_notes: std::collections::VecDeque<Receiver<Response>> = std::collections::VecDeque::new();
    // A clean close and a violation both end the connection, as the match did before.
    while let Ok(Some(text)) = read_frame(&mut stream, MAX_FRAME_BYTES) {
        let request = match server::parse_request(&text) {
            Ok(request) => request,
            Err(response) => {
                if send_response(&mut stream, &response).is_err() {
                    break;
                }
                continue;
            }
        };
        let id = request.id.clone();
        let timeout = server::timeout_for(&request);
        // A request that may cancel a render-out also wakes an app thread waiting on a stalled
        // render-out output, once it is queued, as on the WebSocket path (ADR-2396).
        let wakes = server::may_cancel(&request.method);
        let board_params = board
            .filter(|_| request.method == "render.out_progress" && request.jsonrpc == "2.0")
            .filter(|_| {
                matches!(
                    request.id,
                    Some(serde_json::Value::String(_) | serde_json::Value::Number(_))
                )
            })
            .map(|board| (board, request.params.clone()));
        let (reply_tx, reply_rx) = bounded(1);
        let kept = id.as_ref().map(|_| reply_tx.clone());
        let tracked = connection.track(&reply_tx, &request.method);
        let tracked_reply = reply_tx.clone();
        let forwarded = if tracked {
            server::queue_within(
                inbox,
                Inbound {
                    request,
                    reply: reply_tx,
                },
                board,
            )
        } else {
            server::Forwarded::Busy(server::connection_capacity_error())
        };
        if !matches!(forwarded, server::Forwarded::Sent) {
            server::take_connection(&tracked_reply);
        }
        drop(tracked_reply);
        match forwarded {
            server::Forwarded::Sent => {
                if wakes {
                    crate::render_out::note_cancel_request();
                }
            }
            server::Forwarded::Refused(error) => {
                if send_response(
                    &mut stream,
                    &Response::err(id.unwrap_or(serde_json::Value::Null), error),
                )
                .is_err()
                {
                    break;
                }
                break;
            }
            server::Forwarded::Busy(error) => {
                // A notification gets no answer; a request gets the error, or a poll the
                // board's answer (the inbox is full of what a long step has not drained).
                if let Some(id) = id {
                    let board_answer = board_params
                        .as_ref()
                        .and_then(|(board, params)| board.answer(params));
                    let response = match board_answer {
                        Some(answer) => Response::ok(id, answer),
                        None => Response::err(id, error),
                    };
                    if send_response(&mut stream, &response).is_err() {
                        break;
                    }
                }
                continue;
            }
            server::Forwarded::Closed => break,
        }
        let Some(id) = id else {
            // Notification: no response, but no more than `MAX_QUEUED_NOTIFICATIONS` in flight.
            queued_notes.push_back(reply_rx);
            queued_notes
                .retain(|rx| !matches!(rx.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
            while queued_notes.len() > server::MAX_QUEUED_NOTIFICATIONS {
                if let Some(oldest) = queued_notes.pop_front() {
                    let _ = oldest.recv_timeout(timeout);
                }
            }
            continue;
        };
        let answered = match &board_params {
            Some((board, params)) => server::await_progress(&reply_rx, board, params, timeout, kept.as_ref()),
            None => reply_rx
                .recv_timeout(timeout)
                .ok()
                .map(server::ProgressAnswer::App),
        };
        if let Some(kept) = &kept {
            server::take_connection(kept);
        }
        let response = match answered {
            Some(answer) => answer.with_id(&id),
            None => {
                if let Some(kept) = kept {
                    server::abandon(kept);
                }
                Response::err(
                    id.clone(),
                    crate::protocol::RpcError::new(
                        crate::protocol::TIMEOUT,
                        "F0006",
                        format!("no answer within {} ms", timeout.as_millis()),
                        "the app may be paused by the OS or blocked; pass params.timeout_ms for long operations",
                    ),
                )
            }
        };
        if send_response(&mut stream, &response).is_err() {
            break;
        }
    }
}

fn send_response(stream: &mut LocalStream, response: &Response) -> Result<(), ()> {
    let text = serde_json::to_string(response).map_err(|_| ())?;
    write_frame(stream, text.as_bytes()).map_err(|_| ())
}
