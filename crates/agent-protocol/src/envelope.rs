//! Building the envelopes the generated types describe.
//!
//! Protobuf's `oneof` becomes two levels of Rust `Option` -- an `Envelope`
//! whose `body` may be a `Request` whose `kind` may be a `HelloRequest` -- and
//! writing that out at every call site is how a peer ends up sending an
//! envelope with no body at all. These constructors make the valid shape the
//! easy one.

use crate::v1::{Envelope, Error, ErrorCode, Request, Response, envelope, request, response};

impl Envelope {
    /// A request numbered `request_id`.
    ///
    /// The id must be unique among the requests this peer still expects an
    /// answer to; ids from the two directions are independent and may collide.
    #[must_use]
    pub fn request(request_id: u32, kind: request::Kind) -> Self {
        Self {
            request_id,
            body: Some(envelope::Body::Request(Request { kind: Some(kind) })),
        }
    }

    /// The answer to the request numbered `request_id`.
    #[must_use]
    pub fn response(request_id: u32, kind: response::Kind) -> Self {
        Self {
            request_id,
            body: Some(envelope::Body::Response(Response { kind: Some(kind) })),
        }
    }

    /// The answer to a request that failed.
    ///
    /// `message` is for the peer's log. Anything the peer has to branch on
    /// belongs in `code`.
    #[must_use]
    pub fn error(request_id: u32, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::response(
            request_id,
            response::Kind::Error(Error {
                code: code.into(),
                message: message.into(),
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::HeartbeatRequest;

    #[test]
    fn a_request_carries_its_kind_and_id() {
        let envelope = Envelope::request(3, request::Kind::Heartbeat(HeartbeatRequest {}));

        assert_eq!(envelope.request_id, 3);
        let Some(envelope::Body::Request(request)) = envelope.body else {
            panic!("expected a request body");
        };
        assert!(matches!(request.kind, Some(request::Kind::Heartbeat(_))));
    }

    #[test]
    fn an_error_answers_the_request_that_failed() {
        let envelope = Envelope::error(9, ErrorCode::Unauthenticated, "no session yet");

        assert_eq!(envelope.request_id, 9);
        let Some(envelope::Body::Response(response)) = envelope.body else {
            panic!("expected a response body");
        };
        let Some(response::Kind::Error(error)) = response.kind else {
            panic!("expected an error");
        };
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message, "no session yet");
    }
}
