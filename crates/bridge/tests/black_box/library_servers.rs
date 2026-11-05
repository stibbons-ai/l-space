use api::LibraryServer;

use test_support::bridge::TestBridge;

#[tokio::test]
async fn lists_configured_library_servers_with_normalized_urls_in_file_order() {
    let bridge = TestBridge::start(
        r#"
        [[library_servers]]
        url = "HTTPS://Books.Example.COM:443/"
        name = "Home"

        [[library_servers]]
        url = "http://192.168.1.5:13378/audiobookshelf/"
        "#,
    );

    assert_eq!(
        bridge.api().library_servers().await,
        [
            LibraryServer {
                base_url: "https://books.example.com".into(),
                name: Some("Home".into()),
            },
            LibraryServer {
                base_url: "http://192.168.1.5:13378/audiobookshelf".into(),
                name: None,
            },
        ]
    );
}

#[tokio::test]
async fn lists_no_library_servers_when_none_are_configured() {
    let bridge = TestBridge::start("");

    assert_eq!(bridge.api().library_servers().await, []);
}

/// Pins the wire shape the App decodes, independent of the shared Rust types.
#[tokio::test]
async fn library_server_list_wire_shape() {
    let bridge = TestBridge::start(
        r#"
        [[library_servers]]
        url = "https://abs.lan"
        name = "Home"

        [[library_servers]]
        url = "http://other.lan"
        "#,
    );

    let body: serde_json::Value = bridge.api().get("/api/library-servers").await;
    assert_eq!(
        body,
        serde_json::json!([
            {"base_url": "https://abs.lan", "name": "Home"},
            {"base_url": "http://other.lan", "name": null},
        ])
    );
}
