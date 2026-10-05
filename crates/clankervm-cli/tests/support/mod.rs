//! Shared test doubles for the integration tests.
//!
//! [`FakeAws`] is a real HTTP server: the CLI talks to it through the AWS SDK,
//! so these tests exercise request serialization, signing and error mapping.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

/// How long [`FakeAws::finish`] waits for a scripted request that has not arrived.
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

/// One scripted HTTP response.
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: 404,
            body: r#"{"__type":"ResourceNotFoundException"}"#.into(),
        }
    }

    /// A transient conflict while an image update is still settling.
    pub fn image_updating() -> Self {
        Self {
            status: 409,
            body: r#"{"__type":"ConflictException","message":"The image is currently Updating"}"#
                .into(),
        }
    }

    /// A conflict AWS returns for an update sent while the image is busy.
    pub fn image_already_updating() -> Self {
        Self {
            status: 409,
            body: r#"{"__type":"ConflictException","message":"MicroVM Image is already in state: UPDATING"}"#.into(),
        }
    }

    /// The validation error AWS returns for the same busy image.
    pub fn image_in_current_state() -> Self {
        Self {
            status: 400,
            body: r#"{"__type":"ValidationException","message":"Cannot update MicroVM Image in its current state: arn:aws:lambda:us-east-1:123456789012:microvm-image:demo"}"#.into(),
        }
    }

    /// A 500 whose body is not JSON, as AWS intermittently returns.
    pub fn html_server_error() -> Self {
        Self {
            status: 500,
            body: "<html><body>Internal Server Error</body></html>".into(),
        }
    }

    /// A refusal that carries the error code and message AWS reports.
    pub fn access_denied() -> Self {
        Self {
            status: 403,
            body: r#"{"__type":"AccessDeniedException","message":"not allowed to list MicroVMs"}"#
                .into(),
        }
    }
}

/// A local AWS endpoint that answers scripted responses and records requests.
pub struct FakeAws {
    address: String,
    expected: usize,
    requests: Receiver<String>,
}

impl FakeAws {
    pub fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let expected = responses.len();
        let (sender, requests) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let n = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..n]);
                    if n == 0 || bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers_end = bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < headers_end + length {
                    let read = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                }
                // `finish` may have given up waiting and dropped the receiver.
                let _ = sender.send(String::from_utf8_lossy(&bytes).into_owned());
                write!(
                    stream,
                    "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                )
                .unwrap();
            }
        });
        Self {
            address,
            expected,
            requests,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn finish(self) -> Vec<String> {
        let mut requests = Vec::with_capacity(self.expected);
        while requests.len() < self.expected {
            let request = self
                .requests
                .recv_timeout(FINISH_TIMEOUT)
                .unwrap_or_else(|error| {
                    panic!(
                        "{} of {} scripted responses were never requested ({error}): {requests:#?}",
                        self.expected - requests.len(),
                        self.expected
                    )
                });
            requests.push(request);
        }
        requests
    }
}

/// A listing page with one running MicroVM and a token for the page after it.
pub const MICROVMS_PAGE_RUNNING: &str = r#"{"items":[{"microvmId":"microvm-1","state":"RUNNING","imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"7","startedAt":1787616000}],"nextToken":"page-2"}"#;
/// The page after [`MICROVMS_PAGE_RUNNING`], ending the listing.
pub const MICROVMS_PAGE_TERMINATED: &str = r#"{"items":[{"microvmId":"microvm-2","state":"TERMINATED","imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"7","startedAt":1787529600}]}"#;
/// A listing page with no MicroVMs.
pub const MICROVMS_NONE: &str = r#"{"items":[]}"#;

/// One page of log events for a MicroVM's stream.
pub const LOG_EVENTS: &str = r#"{"events":[{"timestamp":1787616001000,"message":"hello from a MicroVM\n","ingestionTime":1787616001000}],"nextForwardToken":"f/1","nextBackwardToken":"b/1"}"#;

/// The streams one log group holds, as `DescribeLogStreams` reports them.
pub const LOG_STREAMS: &str = r#"{"logStreams":[{"logStreamName":"2026/09/11[13.0]microvm-f4e3b5a1-3a16-3f63-8470-251708859820"}]}"#;

/// An image as `GetMicrovmImage` reports it.
pub const IMAGE_CREATED: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"CREATED","latestActiveImageVersion":"2","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;
/// The same image while its build is still running.
pub const IMAGE_CREATING: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"CREATING","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;

/// The image after the build of version 2 failed.
pub const IMAGE_CREATE_FAILED: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"CREATE_FAILED","latestFailedImageVersion":"2","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;
/// The image while version 3 builds, as `UpdateMicrovmImage` reports it.
pub const IMAGE_UPDATING: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"UPDATING","latestFailedImageVersion":"2","createdAt":1787616000,"updatedAt":1787616100,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"3"}"#;
/// The image once version 3 is active.
pub const IMAGE_UPDATED: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"UPDATED","latestActiveImageVersion":"3","latestFailedImageVersion":"2","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"3"}"#;

/// A page of image versions holding the release just activated and an older one.
pub const VERSIONS_PAGE_ACTIVE: &str = r#"{"items":[{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"SUCCESSFUL","status":"ACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"},{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"1","state":"SUCCESSFUL","status":"INACTIVE","createdAt":1787529600,"baseImageArn":"base","buildRoleArn":"role"}],"nextToken":"versions-2"}"#;
/// The page after [`VERSIONS_PAGE_ACTIVE`], holding a version AWS is already deleting.
pub const VERSIONS_PAGE_DELETED: &str = r#"{"items":[{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"0","state":"DELETED","status":"INACTIVE","createdAt":1787443200,"baseImageArn":"base","buildRoleArn":"role"}]}"#;

/// An image version that AWS reports as active.
pub const VERSION_ACTIVE: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"SUCCESSFUL","status":"ACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"}"#;
/// A failed image version that reports no reason of its own.
pub const VERSION_FAILED: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"FAILED","status":"INACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"}"#;
/// Version 3, active after a retried build.
pub const VERSION_3_ACTIVE: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"3","state":"SUCCESSFUL","status":"ACTIVE","createdAt":1787616100,"baseImageArn":"base","buildRoleArn":"role"}"#;
/// The build records of [`VERSION_FAILED`], one per chipset generation, both carrying the reason.
pub const BUILDS_FAILED: &str = r#"{"items":[{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","buildId":"build-1","buildState":"FAILED","architecture":"ARM_64","chipset":"GRAVITON","chipsetGeneration":"3","stateReason":"Ready hook invocation timed out","createdAt":1787616000},{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","buildId":"build-2","buildState":"FAILED","architecture":"ARM_64","chipset":"GRAVITON","chipsetGeneration":"4","stateReason":"Ready hook invocation timed out","createdAt":1787616000}]}"#;

/// The same image version while its build is still running.
pub const VERSION_PENDING: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"PENDING","status":"INACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"}"#;
