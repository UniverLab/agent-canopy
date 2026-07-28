//! Tests for config module

use super::*;

#[test]
fn test_mcp_server_entry_creation() {
    let server = McpServerEntry {
        name: "test-server".to_string(),
        config: serde_json::json!({
            "url": "http://localhost:8080",
            "type": "http"
        }),
        enabled: true,
    };

    assert_eq!(server.name, "test-server");
    assert_eq!(server.config["url"], "http://localhost:8080");
    assert_eq!(server.config["type"], "http");
    assert!(server.enabled);
}

#[test]
fn test_platform_mcp_config_creation() {
    let servers = vec![McpServerEntry {
        name: "server1".to_string(),
        config: serde_json::json!({"url": "http://localhost:8080"}),
        enabled: true,
    }];

    let config = PlatformMcpConfig {
        platform: "test-platform".to_string(),
        config_path: "/tmp/config.json".to_string(),
        servers,
    };

    assert_eq!(config.platform, "test-platform");
    assert_eq!(config.config_path, "/tmp/config.json");
    assert_eq!(config.servers.len(), 1);
}

#[test]
fn test_mcp_config_registry_new() {
    let registry = McpConfigRegistry::new();
    assert!(registry.platforms.is_empty());
}

#[test]
fn test_unique_server_names() {
    let mut registry = McpConfigRegistry::new();

    let servers1 = vec![McpServerEntry {
        name: "server1".to_string(),
        config: serde_json::json!({"url": "http://localhost:8080"}),
        enabled: true,
    }];

    let servers2 = vec![
        McpServerEntry {
            name: "server2".to_string(),
            config: serde_json::json!({"url": "http://localhost:8081"}),
            enabled: true,
        },
        McpServerEntry {
            name: "server1".to_string(),
            config: serde_json::json!({"url": "http://localhost:8080"}),
            enabled: true,
        },
    ];

    registry.platforms.push(PlatformMcpConfig {
        platform: "platform1".to_string(),
        config_path: "/tmp/config1.json".to_string(),
        servers: servers1,
    });

    registry.platforms.push(PlatformMcpConfig {
        platform: "platform2".to_string(),
        config_path: "/tmp/config2.json".to_string(),
        servers: servers2,
    });

    let unique_names = registry.unique_server_names();
    assert_eq!(unique_names.len(), 2);
    assert!(unique_names.contains(&"server1"));
    assert!(unique_names.contains(&"server2"));
}

#[test]
fn test_server_diff() {
    let mut registry = McpConfigRegistry::new();

    let servers1 = vec![
        McpServerEntry {
            name: "server1".to_string(),
            config: serde_json::json!({"url": "http://localhost:8080"}),
            enabled: true,
        },
        McpServerEntry {
            name: "server2".to_string(),
            config: serde_json::json!({"url": "http://localhost:8081"}),
            enabled: true,
        },
    ];

    let servers2 = vec![McpServerEntry {
        name: "server2".to_string(),
        config: serde_json::json!({"url": "http://localhost:8081"}),
        enabled: true,
    }];

    registry.platforms.push(PlatformMcpConfig {
        platform: "platform1".to_string(),
        config_path: "/tmp/config1.json".to_string(),
        servers: servers1,
    });

    registry.platforms.push(PlatformMcpConfig {
        platform: "platform2".to_string(),
        config_path: "/tmp/config2.json".to_string(),
        servers: servers2,
    });

    let diff = registry.server_diff("platform1", "platform2");
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].name, "server1");
}
