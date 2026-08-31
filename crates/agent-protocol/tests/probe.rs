//! What a probe report has to survive on the wire.
//!
//! The verdict is what a later task derives a GPU status from and the checks
//! are what the host logs, so the shape they arrive in is worth a test of its
//! own: a report is written by a guest that may be older or newer than the
//! host reading it.

use prost::Message;
use vmlord_agent_protocol::{
    handshake::CURRENT_VERSION,
    v1::{
        Envelope, GpuProbeCheck, GpuProbeCheckState, GpuProbeStep, GpuProbeVerdict,
        ProbeGpuResponse, envelope, response,
    },
};

// The version is a constant, so the floor below folds to `true`; what the
// assertion guards is a future edit that lowers it.
#[allow(clippy::assertions_on_constants)]
#[test]
fn a_probe_report_belongs_to_revision_one_five_or_later() {
    // Messages and enum values only, so an agent from 1.4 is simply never
    // asked and a host from 1.4 never has to read one. Later minors add more
    // of the same kind, which is why this is a floor rather than an equality:
    // the revision that introduced the report is 1.5 and nothing since has
    // changed what one means.
    assert_eq!(CURRENT_VERSION.major, 1);
    assert!(
        CURRENT_VERSION.minor >= 5,
        "probe reports exist from 1.5 onwards, not in {CURRENT_VERSION:?}"
    );
}

#[test]
fn a_probe_report_survives_the_round_trip() {
    let report = Envelope::response(
        11,
        response::Kind::ProbeGpu(ProbeGpuResponse {
            verdict: i32::from(GpuProbeVerdict::Renders),
            checks: vec![
                GpuProbeCheck {
                    step: i32::from(GpuProbeStep::Device),
                    state: i32::from(GpuProbeCheckState::Ok),
                    message: "/dev/dxg is a usable device".to_owned(),
                },
                GpuProbeCheck {
                    step: i32::from(GpuProbeStep::Vulkan),
                    state: i32::from(GpuProbeCheckState::Failed),
                    message: "vulkaninfo named no device".to_owned(),
                },
            ],
            renderer: "D3D12 (NVIDIA GeForce RTX 4070)".to_owned(),
            driver: "dxgkrnl".to_owned(),
            render_node: String::new(),
        }),
    );

    let decoded = Envelope::decode(report.encode_to_vec().as_slice()).expect("a decodable report");
    assert_eq!(decoded.request_id, 11);
    let Some(envelope::Body::Response(response)) = decoded.body else {
        panic!("a report is a response");
    };
    let Some(response::Kind::ProbeGpu(report)) = response.kind else {
        panic!("a report is a probe report");
    };
    assert_eq!(report.verdict(), GpuProbeVerdict::Renders);
    assert_eq!(report.checks[0].step(), GpuProbeStep::Device);
    assert_eq!(report.checks[0].state(), GpuProbeCheckState::Ok);
    assert_eq!(report.checks[1].state(), GpuProbeCheckState::Failed);
    assert_eq!(report.renderer, "D3D12 (NVIDIA GeForce RTX 4070)");
}
