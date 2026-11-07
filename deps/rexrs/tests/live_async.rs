//! Tests against a running Rex server, over the async client. See `tests/live.rs`.

use std::time::Duration;

use rexrs::api::{self, BlockMethod, Layout, session, terminal};
use rexrs::{ApiError, AsyncRexClient, Error};

#[tokio::test]
#[ignore = "needs a running Rex server"]
async fn async_session_round_trip() {
    let mut client = AsyncRexClient::builder()
        .name("rexrs-tests")
        .connect()
        .await
        .unwrap();
    assert_eq!(client.welcome().api_revision, api::API_REVISION);

    let created = client
        .call(
            session::Create::new()
                .label(format!("rexrs-test-async-{}", std::process::id()))
                .initial_windows([session::CreateInitialWindow::new().layout(Layout::shell())]),
        )
        .await
        .unwrap();
    let session_id = created.session_id;
    let block_id = created.initial_windows[0].block_ids[0].clone();

    let result = async {
        client.call(session::Attach::new(&session_id)).await?;
        client
            .call(terminal::Write::new("echo rexrs-async\n").on(&session_id, &block_id))
            .await?;
        for _ in 0..100 {
            let screen = client
                .call(terminal::Format::new().on(&session_id, &block_id))
                .await?;
            if screen.content.contains("rexrs-async") {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(Error::Protocol("output never appeared".into()))
    }
    .await;

    client
        .call(session::Destroy::new(&session_id))
        .await
        .unwrap();
    // The session's own events, such as session_destroyed, arrived while we waited.
    assert!(
        client
            .drain_events()
            .any(|event| event.session_id.as_deref() == Some(session_id.as_str()))
    );
    result.unwrap();

    let gone = client.call(session::View::new(&session_id)).await;
    assert!(
        matches!(gone, Err(Error::Api(ApiError::SessionNotFound { .. }))),
        "{gone:?}"
    );
    client.close().await.unwrap();
}
