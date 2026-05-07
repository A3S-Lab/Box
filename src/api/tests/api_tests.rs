//! Integration tests for Docker Engine API
//!
//! These tests verify that all API endpoints work correctly.

use serde_json::json;

#[tokio::test]
async fn test_system_endpoints() {
    // Test ping endpoint
    let result = a3s_box_api::handlers::system::ping().await;
    // Ping returns impl IntoResponse, just verify it doesn't panic

    // Test version endpoint
    let version = a3s_box_api::handlers::system::version().await;
    assert!(version.is_ok());

    // Test info endpoint
    let info = a3s_box_api::handlers::system::info().await;
    assert!(info.is_ok());
}

#[tokio::test]
async fn test_network_list() {
    let result = a3s_box_api::handlers::networks::list().await;
    assert!(result.is_ok());

    let json = result.unwrap().0;
    assert!(json.is_array());

    // Should return 3 default networks (bridge, host, none)
    let networks = json.as_array().unwrap();
    assert_eq!(networks.len(), 3);
}

#[tokio::test]
async fn test_network_inspect_bridge() {
    use axum::extract::Path;

    let result = a3s_box_api::handlers::networks::inspect(
        Path("bridge".to_string())
    ).await;

    assert!(result.is_ok());
    let json = result.unwrap().0;
    assert_eq!(json["Name"], "bridge");
}

#[tokio::test]
async fn test_network_inspect_nonexistent() {
    use axum::extract::Path;

    let result = a3s_box_api::handlers::networks::inspect(
        Path("nonexistent".to_string())
    ).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_container_list() {
    use axum::extract::Query;

    let query = a3s_box_api::handlers::containers::ListQuery::default();
    let result = a3s_box_api::handlers::containers::list(Query(query)).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_image_list() {
    use axum::extract::Query;

    let query = a3s_box_api::handlers::images::ListQuery::default();
    let result = a3s_box_api::handlers::images::list(Query(query)).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_volume_list() {
    let result = a3s_box_api::handlers::volumes::list().await;
    assert!(result.is_ok());

    let json = result.unwrap().0;
    assert!(json["Volumes"].is_array());
}

#[tokio::test]
async fn test_events_endpoint() {
    use axum::extract::Query;

    let query = a3s_box_api::handlers::system::EventsQuery::default();
    let result = a3s_box_api::handlers::system::events(Query(query)).await;

    assert!(result.is_ok());
}

// Test error cases
#[tokio::test]
async fn test_container_inspect_nonexistent() {
    use axum::extract::Path;

    let result = a3s_box_api::handlers::containers::inspect(
        Path("nonexistent123".to_string())
    ).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_image_inspect_nonexistent() {
    use axum::extract::Path;

    let result = a3s_box_api::handlers::images::inspect(
        Path("nonexistent:latest".to_string())
    ).await;

    assert!(result.is_err());
}
