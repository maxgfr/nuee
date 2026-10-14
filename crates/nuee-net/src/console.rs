//! Framing of console requests on `nuee/http/0`, shared by the hosts and every client.
//!
//! One bi-stream per request. The client first sends a small hello frame, postcard
//! `TcpMsg::Hello{version, proof}` (at most [`MAX_CONSOLE_HELLO`] bytes), then one
//! `HttpRequest` frame (at most `MAX_HTTP_MSG` bytes), and finishes its side. The host
//! checks the hello's proof before it reads, let alone allocates, the request frame.

use iroh::endpoint::{RecvStream, SendStream};
use nuee_proto::{
    EndpointId, HttpRequest, MAX_HTTP_MSG, PROOF_HTTP, PROTO_VERSION, TcpMsg, decode_with_limit,
    encode_with_limit,
};

use crate::admit::{Admission, AdmitError};
use crate::framing::{FrameError, read_frame, recv_msg, send_msg, write_frame};

/// Largest console hello frame.
pub const MAX_CONSOLE_HELLO: usize = 64;

/// Sends `req` as a console request: the hello carrying `req.proof`, then the request.
pub async fn send_console_request(
    send: &mut SendStream,
    req: &HttpRequest,
) -> Result<(), FrameError> {
    let body = encode_with_limit(req, MAX_HTTP_MSG)?;
    let hello = TcpMsg::Hello {
        version: PROTO_VERSION,
        proof: req.proof,
    };
    send_msg(send, &hello).await?;
    write_frame(send, &body).await
}

/// Reads and checks the console hello of `peer` under the gate's hello deadline, holding a
/// pending slot only for that time. On success the caller reads the request with
/// [`recv_console_body`].
pub async fn recv_console_hello(
    recv: &mut RecvStream,
    gate: &Admission,
    secret: &[u8],
    peer: &EndpointId,
    own: &EndpointId,
) -> Result<[u8; 32], AdmitError> {
    let _pending = gate.begin(peer)?;
    let checked = gate
        .within(peer, async {
            match recv_msg::<TcpMsg>(recv, MAX_CONSOLE_HELLO).await? {
                TcpMsg::Hello { version, proof } => {
                    if version != PROTO_VERSION {
                        return Err(AdmitError::BadVersion(version));
                    }
                    gate.verify(PROOF_HTTP, secret, peer, own, &proof)?;
                    Ok(proof)
                }
                _ => Err(AdmitError::Unexpected),
            }
        })
        .await;
    if let Err(AdmitError::Frame(_) | AdmitError::Unexpected) = &checked {
        gate.record_failure(peer);
    }
    checked
}

/// Reads the request that follows an accepted hello (its own `proof` field is not used:
/// the hello's was checked).
pub async fn recv_console_body(recv: &mut RecvStream) -> Result<HttpRequest, FrameError> {
    let bytes = read_frame(recv, MAX_HTTP_MSG).await?;
    Ok(decode_with_limit(&bytes, MAX_HTTP_MSG)?)
}
