use super::*;
use crate::test_support::project;
use tempfile::TempDir;

#[test]
fn endpoint_parsing_and_paths() {
    for path in [
        "//evil",
        "https://evil",
        "/\\evil",
        "/\n",
        "/#fragment",
        "relative",
    ] {
        assert!(validate_path(path).is_err());
    }
    let base = endpoint("test.lambda-microvm.us-east-1.on.aws:443").unwrap();
    assert_eq!(base.scheme(), "https");
    assert_eq!(
        endpoint("https://test.lambda-microvm.us-east-1.on.aws")
            .unwrap()
            .host_str(),
        Some("test.lambda-microvm.us-east-1.on.aws")
    );
    for (protocol, scheme) in [(Protocol::Http, "https"), (Protocol::Websocket, "wss")] {
        let url = application_url(&base, "/api?x=1", protocol).unwrap();
        assert_eq!(url.port_or_known_default(), Some(443));
        assert_eq!(url.scheme(), scheme);
    }
}

#[allow(clippy::manual_assert_eq)]
#[test]
fn templates_expand_only_configured_arguments() {
    let token = AuthToken::new("sentinel".into()).unwrap();
    assert!(!format!("{token:?}").contains("sentinel"));
    let url = Url::parse("https://test.lambda-microvm.us-east-1.on.aws/").unwrap();
    let template = ["curl", "{url}", "{auth-header}", "{port-header}"].map(str::to_owned);
    for port in [3000, 3001] {
        let args = client_arguments(
            &template,
            &url,
            &token,
            port,
            &["{auth-header}".into(), String::new()],
        );
        assert!(args[1] == "X-aws-proxy-auth: sentinel");
        assert_eq!(args[2], format!("X-aws-proxy-port: {port}"));
        assert_eq!(&args[3..], ["{auth-header}", ""]);
    }
    for invalid in [
        "",
        "secret\r\nInjected: yes",
        "secret token",
        "secret=",
        "秘密",
    ] {
        assert!(AuthToken::new(invalid.into()).is_err());
    }
}
