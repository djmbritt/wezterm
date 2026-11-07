use rexrs::api::session;
use serde_json::json;

#[test]
fn empty_attachment_list_is_not_default_selection() {
    let default = serde_json::to_value(session::Bootstrap::new("session")).unwrap();
    assert!(default.get("attach_session_ids").is_none());
    let none = serde_json::to_value(
        session::Bootstrap::new("session").attach_session_ids(Vec::<String>::new()),
    )
    .unwrap();
    assert_eq!(none["attach_session_ids"], json!([]));
}
