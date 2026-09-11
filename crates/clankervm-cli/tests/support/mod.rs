//! Shared test doubles for the integration tests.
//!
//! [`FakeAws`] is a real HTTP server: the CLI talks to it through the AWS SDK,
//! so these tests exercise request serialization, signing and error mapping.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

/// One scripted HTTP response.
pub struct Response {
    pub status: u16,
    pub body: &'static str,
}

impl Response {
    pub fn ok(body: &'static str) -> Self {
        Self { status: 200, body }
    }

    pub fn not_found() -> Self {
        Self {
            status: 404,
            body: r#"{"__type":"ResourceNotFoundException"}"#,
        }
    }

    /// A refusal that carries the error code and message AWS reports.
    pub fn access_denied() -> Self {
        Self {
            status: 403,
            body: r#"{"__type":"AccessDeniedException","message":"not allowed to list MicroVMs"}"#,
        }
    }
}

/// A local AWS endpoint that answers scripted responses and records requests.
pub struct FakeAws {
    address: String,
    join: thread::JoinHandle<Vec<String>>,
}

impl FakeAws {
    pub fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let join = thread::spawn(move || {
            let mut requests = Vec::new();
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
                requests.push(String::from_utf8_lossy(&bytes).into_owned());
                write!(
                    stream,
                    "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                )
                .unwrap();
            }
            requests
        });
        Self { address, join }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Waits for the scripted requests and returns them.
    pub fn finish(self) -> Vec<String> {
        self.join.join().unwrap()
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
pub const LOG_STREAMS: &str =
    r#"{"logStreams":[{"logStreamName":"job-1"},{"logStreamName":"job-2"}]}"#;

/// An image as `GetMicrovmImage` reports it.
pub const IMAGE_CREATED: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"CREATED","latestActiveImageVersion":"2","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;
/// The same image while its build is still running.
pub const IMAGE_CREATING: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","name":"demo","state":"CREATING","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role","imageVersion":"2"}"#;

/// A page of image versions holding the release just activated and an older one.
pub const VERSIONS_PAGE_ACTIVE: &str = r#"{"items":[{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"SUCCESSFUL","status":"ACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"},{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"1","state":"SUCCESSFUL","status":"INACTIVE","createdAt":1787529600,"baseImageArn":"base","buildRoleArn":"role"}],"nextToken":"versions-2"}"#;
/// The page after [`VERSIONS_PAGE_ACTIVE`], holding a version AWS is already deleting.
pub const VERSIONS_PAGE_DELETED: &str = r#"{"items":[{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"0","state":"DELETED","status":"INACTIVE","createdAt":1787443200,"baseImageArn":"base","buildRoleArn":"role"}]}"#;

/// An image version that AWS reports as active.
pub const VERSION_ACTIVE: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"SUCCESSFUL","status":"ACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"}"#;
/// The same image version while its build is still running.
pub const VERSION_PENDING: &str = r#"{"imageArn":"arn:aws:lambda:us-east-1:123456789012:microvm-image:demo","imageVersion":"2","state":"PENDING","status":"INACTIVE","createdAt":1787616000,"baseImageArn":"base","buildRoleArn":"role"}"#;
