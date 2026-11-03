use api::LibraryServer;

use crate::support::TestBridge;

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
