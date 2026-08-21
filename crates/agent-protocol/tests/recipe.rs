//! What a recipe report has to survive on the wire.
//!
//! The stages are what the host logs and what a later task will derive a
//! status from, so the shape they arrive in is worth a test of its own: a
//! report is written by a guest that may be older or newer than the host
//! reading it.

use prost::Message;
use vmlord_agent_protocol::v1::{
    ApplyGpuRecipeRequest, ApplyGpuRecipeResponse, Envelope, GpuRecipeStage, GpuRecipeStageState,
    GpuRecipeStep, envelope, request, response,
};

#[test]
fn a_recipe_report_survives_the_round_trip() {
    let report = Envelope::response(
        7,
        response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse {
            stages: vec![
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Distribution),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "ubuntu 26.04 amd64".to_owned(),
                },
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::ModuleBuild),
                    state: i32::from(GpuRecipeStageState::Failed),
                    message: "dkms build failed".to_owned(),
                },
            ],
        }),
    );

    let decoded = Envelope::decode(report.encode_to_vec().as_slice()).expect("a decodable report");
    assert_eq!(decoded.request_id, 7);
    let Some(envelope::Body::Response(response)) = decoded.body else {
        panic!("a report is a response");
    };
    let Some(response::Kind::ApplyGpuRecipe(report)) = response.kind else {
        panic!("a report is a recipe report");
    };
    assert_eq!(report.stages.len(), 2);
    assert_eq!(report.stages[0].step(), GpuRecipeStep::Distribution);
    assert_eq!(report.stages[1].state(), GpuRecipeStageState::Failed);
}

#[test]
fn an_apply_request_carries_nothing_and_still_arrives() {
    // Empty on purpose: everything the guest needs is in the guest or in the
    // payload it was told to mount. The request must therefore survive as an
    // arm rather than as bytes -- an empty message encodes to nothing.
    let request = Envelope::request(3, request::Kind::ApplyGpuRecipe(ApplyGpuRecipeRequest {}));

    let decoded =
        Envelope::decode(request.encode_to_vec().as_slice()).expect("a decodable request");
    let Some(envelope::Body::Request(request)) = decoded.body else {
        panic!("an apply is a request");
    };
    assert!(matches!(
        request.kind,
        Some(request::Kind::ApplyGpuRecipe(_))
    ));
}

#[test]
fn the_userspace_steps_travel_beside_the_kernel_ones() {
    // Enum values only, so an agent from an older minor simply never sends
    // these and a host from one logs a step it has no name for rather than
    // misreading one. What revision this build speaks is asserted once, in
    // `probe.rs`, beside the newest thing that moved it.
    let report = Envelope::response(
        9,
        response::Kind::ApplyGpuRecipe(ApplyGpuRecipeResponse {
            stages: vec![
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Userspace),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "staged mesa from the payload".to_owned(),
                },
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::VulkanIcd),
                    state: i32::from(GpuRecipeStageState::Skipped),
                    message: "the payload carries no Vulkan driver".to_owned(),
                },
                GpuRecipeStage {
                    step: i32::from(GpuRecipeStep::Environment),
                    state: i32::from(GpuRecipeStageState::Ok),
                    message: "wrote the generator and the profile script".to_owned(),
                },
            ],
        }),
    );

    let decoded = Envelope::decode(report.encode_to_vec().as_slice()).expect("a decodable report");
    let Some(envelope::Body::Response(response)) = decoded.body else {
        panic!("a report is a response");
    };
    let Some(response::Kind::ApplyGpuRecipe(report)) = response.kind else {
        panic!("a report is a recipe report");
    };
    assert_eq!(report.stages[0].step(), GpuRecipeStep::Userspace);
    assert_eq!(report.stages[1].step(), GpuRecipeStep::VulkanIcd);
    assert_eq!(report.stages[2].step(), GpuRecipeStep::Environment);
}
