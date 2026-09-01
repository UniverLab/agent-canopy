#![allow(dead_code)]
#![allow(clippy::doc_markdown)]

mod setup_module {
    #[derive(Clone)]
    pub struct PlatformWithCli {
        pub cli: Option<crate::domain::cli_config::CliConfig>,
    }

    pub mod models {
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
        pub struct Platform {
            pub name: String,
            pub config_path: String,
            #[serde(default)]
            pub config_format: Option<String>,
            #[serde(default)]
            pub toml_array_format: bool,
            #[serde(default = "default_command_format")]
            pub command_format: String,
            #[serde(alias = "servers_key")]
            pub mcp_servers_key: Vec<String>,
            #[serde(default)]
            pub deprecated_keys: Vec<String>,
            #[serde(default)]
            pub unsupported_keys: Vec<String>,
            #[serde(default)]
            pub fields_mapping: std::collections::HashMap<String, String>,
            #[serde(default)]
            pub required_fields: std::collections::HashMap<String, Vec<String>>,
            #[serde(default)]
            pub server_extras: std::collections::HashMap<String, serde_json::Value>,
            #[serde(default)]
            pub skills_dir: Option<String>,
            #[serde(default)]
            pub instruction_file: Option<String>,
            #[serde(default)]
            pub cli: Option<serde_json::Value>,
        }

        fn default_command_format() -> String {
            "separate".to_string()
        }
    }
}

#[path = "../src/domain/mod.rs"]
mod domain;
#[path = "../src/rag/embedding_client.rs"]
mod embedding_client;
#[path = "../src/rag/ort_runtime.rs"]
mod ort_runtime;
#[path = "../src/rag/vector_store.rs"]
mod vector_store;

/// Example: Search the personal RAG for content about denoising metrics
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or(anyhow::anyhow!("Could not find home directory"))?;
    let data_dir = home.join(".canopy");

    // Load config to get model
    let config = domain::canopy_config::CanopyConfig::load(&data_dir);
    let model = config.embeddings_model.trim();

    println!("🔍 RAG Search Example");
    println!("  Model: {}", model);

    // Get embedding dimensions
    let dimensions = embedding_client::model_dimensions(model)
        .map_err(|e| anyhow::anyhow!("Invalid model: {}", e))?;
    println!("  Dimensions: {}", dimensions);

    // Create embedding client
    let client = embedding_client::client_from_config(&config)?;

    // Embed the query (run in blocking task to avoid blocking async executor)
    let query = "métricas validar denoising resultados conclusiones metrics";
    println!("\n🔎 Query: \"{}\"", query);

    let query_vec: Vec<f32> = tokio::task::spawn_blocking({
        let q = query.to_string();
        move || client.embed(&q)
    })
    .await??;

    println!("✅ Query embedded: {} dims\n", query_vec.len());

    // Open vector store and search
    let store: vector_store::VectorStore =
        vector_store::VectorStore::new(dimensions, Some(config.rag_vector_cache_entries)).await?;
    let results: Vec<vector_store::SearchResult> = store.search_similar(&query_vec, 5).await?;

    println!("📊 Top 5 results:\n");
    if results.is_empty() {
        println!("  (No results found)");
    } else {
        for (i, result) in results.iter().enumerate() {
            let filename = std::path::Path::new(&result.file_path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let distance = result.distance.unwrap_or(0.0);

            println!("  [{}] {} (distance: {:.4})", i + 1, filename, distance);
            let content_len = result.content.len();
            let preview_len = content_len.min(150);
            println!("      Content: {}...\n", &result.content[..preview_len]);
        }
    }

    Ok(())
}
