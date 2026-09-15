use super::*;
use crate::client::{AuthToken, Call, FakeMicroVmClient};
use crate::config::ProjectConfig;
use crate::test_support::{MicroVmDetailsBuilder, ROLE, executable, project};
use std::fs;
use tempfile::TempDir;

// Credential comparisons must not print their operands on failure.
#[allow(clippy::manual_assert_eq)]
#[tokio::test]
async fn failed_client_retries_with_fresh_tokens_and_literal_arguments() {
    let mut test = ConnectionTest::new(
        "printf '%s\\0' \"$@\" >> \"$0.arguments\"\n[ -e \"$0.attempt\" ] && exit 0\ntouch \"$0.attempt\"\nexit 37",
    );

    test.client = test.client.auth_tokens([
        AuthToken::new("first-secret".into()),
        AuthToken::new("second-secret".into()),
    ]);

    let args = ["space value", "--leading", "x=y", "", "{url}"].map(str::to_owned);
    let start = Instant::now();

    test.run(&args).await.unwrap();

    assert!(start.elapsed() >= Duration::from_secs(1));
    test.assert_token_calls(2);
    let captured = fs::read(test.dir.path().join("test-client.arguments")).unwrap();
    let arguments: Vec<_> = captured.split(|byte| *byte == 0).collect();
    assert!(arguments[1] == b"X-aws-proxy-auth: first-secret");
    assert!(arguments[9] == b"X-aws-proxy-auth: second-secret");
    assert_eq!(
        arguments[0],
        b"https://test.lambda-microvm.us-east-1.on.aws/"
    );
    assert_eq!(arguments[2], b"X-aws-proxy-port: 3000");
    for offset in [3, 11] {
        assert_eq!(
            &arguments[offset..offset + args.len()],
            args.iter().map(String::as_bytes).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn retry_exhaustion_preserves_exit_status() {
    let test = ConnectionTest::new("exit 37");
    let start = Instant::now();

    assert!(matches!(
        test.run(&[]).await,
        Err(ClankerError::ClientExit(37))
    ));

    assert!(start.elapsed() >= Duration::from_secs(2));
    test.assert_token_calls(2);
}

#[tokio::test]
async fn retry_deadline_does_not_kill_a_running_client_or_restart_it_afterwards() {
    for code in [0, 37] {
        let mut test = ConnectionTest::new(&format!("sleep 1.2\nexit {code}"));
        test.config.connections.get_mut("web").unwrap().timeout = Duration::from_secs(1);

        let result = test.run(&[]).await;

        if code == 0 {
            result.unwrap();
        } else {
            assert!(matches!(result, Err(ClankerError::ClientExit(37))));
        }
        test.assert_token_calls(1);
    }
}

#[tokio::test]
async fn spawn_failure_is_not_retried() {
    let test = ConnectionTest::new("exit 0");
    fs::remove_file(test.dir.path().join("test-client")).unwrap();

    assert!(test.run(&[]).await.unwrap_err().to_string().contains("run"));

    test.assert_token_calls(1);
}

#[tokio::test]
async fn signal_exit_retries_until_the_deadline() {
    let test = ConnectionTest::new("kill -INT $$");

    assert!(matches!(
        test.run(&[]).await,
        Err(ClankerError::ClientExit(130))
    ));

    test.assert_token_calls(2);
}

#[tokio::test(start_paused = true)]
async fn endpoint_wait_handles_delayed_registration_but_not_missing_existing_vms() {
    let client = FakeMicroVmClient::default().described([
        Ok(None),
        Ok(Some(
            MicroVmDetailsBuilder::new("vm")
                .state(MicrovmState::Pending)
                .build(),
        )),
        Ok(Some(
            MicroVmDetailsBuilder::new("vm")
                .application()
                .endpoint("")
                .build(),
        )),
        Ok(Some(MicroVmDetailsBuilder::new("vm").application().build())),
    ]);
    let start = Instant::now();

    wait_for_endpoint(&client, "vm", true, start + Duration::from_secs(10))
        .await
        .unwrap();

    assert_eq!(start.elapsed(), Duration::from_secs(3));
    let client = FakeMicroVmClient::default();
    assert!(matches!(
        wait_for_endpoint(
            &client,
            "vm",
            false,
            Instant::now() + Duration::from_secs(10)
        )
        .await,
        Err(ClankerError::MicroVmNotFound(_))
    ));
    assert_eq!(client.calls(), [Call::Describe("vm".into())]);
}

struct ConnectionTest {
    dir: TempDir,
    config: ProjectConfig,
    client: FakeMicroVmClient,
}

impl ConnectionTest {
    fn new(body: &str) -> Self {
        let dir = TempDir::new().unwrap();
        executable(dir.path(), body);
        let config = project(
            dir.path(),
            &format!(
                "[microvm.run]\n{ROLE}command=['/remote-not-installed']\n[microvm.run.network]\ningress='ALL_INGRESS'\n[microvm.run.connect.web]\nport=3000\nprotocol='http'\ntimeout='2s'\nclient=['./test-client', '{{url}}', '{{auth-header}}', '{{port-header}}']"
            ),
        );
        Self {
            dir,
            config,
            client: FakeMicroVmClient::default().described([Ok(Some(
                MicroVmDetailsBuilder::new("microvm-fake")
                    .application()
                    .build(),
            ))]),
        }
    }

    async fn run(&self, arguments: &[String]) -> Result<(), ClankerError> {
        let options = ConnectOptions {
            vm_id: "microvm-fake".into(),
            name: "web".into(),
            connect_timeout: None,
            arguments: arguments.to_vec(),
        };
        connect(
            options,
            &self.config,
            crate::OutputFormat::Human,
            &self.client,
        )
        .await
    }

    fn assert_token_calls(&self, expected: usize) {
        assert_eq!(
            self.client
                .calls()
                .iter()
                .filter(|call| {
                    matches!(
                        call,
                        Call::AuthToken(_, 3000, expiration)
                            if *expiration == AUTH_TOKEN_EXPIRATION
                    )
                })
                .count(),
            expected
        );
    }
}
