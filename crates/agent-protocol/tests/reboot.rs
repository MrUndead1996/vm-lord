//! What a reboot request has to survive on the wire.
//!
//! Both messages are empty on purpose, the way the heartbeat is: the guest
//! knows how to reboot itself better than the host could tell it to. The
//! answer means the reboot was accepted and initiated, not that the guest has
//! come back, and a refusal travels as the universal `Error` arm.

use prost::Message;
use vmlord_agent_protocol::v1::{
    Envelope, RebootRequest, RebootResponse, envelope, request, response,
};

#[test]
fn a_reboot_request_carries_nothing_and_still_arrives() {
    let request = Envelope::request(4, request::Kind::Reboot(RebootRequest {}));

    let decoded =
        Envelope::decode(request.encode_to_vec().as_slice()).expect("a decodable request");
    let Some(envelope::Body::Request(request)) = decoded.body else {
        panic!("a reboot is a request");
    };
    assert_eq!(decoded.request_id, 4);
    assert!(matches!(request.kind, Some(request::Kind::Reboot(_))));
}

#[test]
fn a_reboot_answer_carries_nothing_and_still_arrives() {
    let answer = Envelope::response(4, response::Kind::Reboot(RebootResponse {}));

    let decoded = Envelope::decode(answer.encode_to_vec().as_slice()).expect("a decodable answer");
    let Some(envelope::Body::Response(answer)) = decoded.body else {
        panic!("a reboot answer is a response");
    };
    assert_eq!(decoded.request_id, 4);
    assert!(matches!(answer.kind, Some(response::Kind::Reboot(_))));
}
