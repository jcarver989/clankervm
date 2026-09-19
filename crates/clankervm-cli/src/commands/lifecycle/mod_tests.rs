use super::*;
use crate::client::FakeMicroVmClient;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn cleanup_timeout_bounds_the_entire_stop_operation() {
    let client = FakeMicroVmClient::default().with_delay(Duration::from_secs(40));
    let start = Instant::now();

    assert!(terminate(&client, "vm-1").await.is_err());

    assert_eq!(start.elapsed(), CLEANUP_TIMEOUT);
}
