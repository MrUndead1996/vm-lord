//! Resolving a release against a server that behaves like Canonical's.

mod support;

use support::{Behaviour, TestServer};
use vmlord_core::ubuntu;
use vmlord_image::{DistroProfile, ResolveError, resolve_image};

const FIXTURE: &str = include_str!("fixtures/ubuntu-24.04-SHA256SUMS");

/// A profile pointing at the loopback server instead of the internet.
///
/// The profile is the seam: because the base URL is data, the test needs no
/// stubbed-out HTTP client, and the code under test is the same code that runs
/// in production.
///
/// The port is only known at run time, which the owned template takes in its
/// stride.
fn profile_for(server: &TestServer) -> DistroProfile {
    DistroProfile {
        directory_template: format!("{}{{release}}/", server.base_url()),
        ..ubuntu()
    }
}

#[test]
fn a_release_resolves_to_the_image_url_and_the_checksum_published_beside_it() {
    let server = TestServer::start(FIXTURE.as_bytes().to_vec(), Behaviour::IgnoresRange);
    let profile = profile_for(&server);

    let resolved = resolve_image(&profile, "24.04").expect("the fixture lists this image");

    assert_eq!(
        resolved.url,
        format!(
            "{}24.04/ubuntu-24.04-server-cloudimg-amd64.img",
            server.base_url()
        )
    );
    assert_eq!(
        resolved.sha256,
        "0533b0655c32e68b31d792ecd6ccfca95abdbc536c4446874fe0513bd4140ffe"
    );
    assert_eq!(resolved.default_user, "ubuntu");
    assert_eq!(resolved.admin_group, "sudo");
}

#[test]
fn a_server_without_that_release_is_reported_as_the_status_it_sent() {
    let server = TestServer::start(Vec::new(), Behaviour::NotFound);
    let profile = profile_for(&server);

    let error = resolve_image(&profile, "24.04").expect_err("404 is not a checksum list");

    assert!(
        matches!(error, ResolveError::UnexpectedStatus { status: 404 }),
        "got {error:?}"
    );
}

#[test]
fn a_release_of_the_wrong_shape_never_reaches_the_network() {
    let server = TestServer::start(FIXTURE.as_bytes().to_vec(), Behaviour::IgnoresRange);
    let profile = profile_for(&server);

    let error = resolve_image(&profile, "../../etc").expect_err("that is not a release");

    assert!(
        matches!(error, ResolveError::InvalidRelease(_)),
        "got {error:?}"
    );
    assert!(
        server.ranges_seen().is_empty(),
        "the request must be refused before a socket is opened"
    );
}
