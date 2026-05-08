/// Example: Search the personal RAG for content about denoising metrics
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or(anyhow::anyhow!("Could not find home directory"))?;
    let data_dir = home.join(".canopy");

    // Load config to get model
    let config = harness_canopy::domain::canopy_config::CanopyConfig::load(&data_dir);
    let model = config.embeddings_model.trim();

    println!("🔍 RAG Search Example");
    println!("  Model: {}", model);

    // Get embedding dimensions
    let dimensions = harness_canopy::rag::embedding_client::model_dimensions(model)
        .map_err(|e| anyhow::anyhow!("Invalid model: {}", e))?;
    println!("  Dimensions: {}", dimensions);

    // Create embedding client
    let client = harness_canopy::rag::embedding_client::client_from_config(&config)?;

    // Embed the query (run in blocking task to avoid blocking async executor)
    let query = "métricas validar denoising resultados conclusiones metrics";
    println!("\n🔎 Query: \"{}\"", query);

    let query_vec: Vec<f32> = tokio::task::spawn_blocking({
        let client = client.clone();
        let q = query.to_string();
        move || client.embed(&q)
    })
    .await??;

    println!("✅ Query embedded: {} dims\n", query_vec.len());

    // Open vector store and search
    let store: harness_canopy::rag::vector_store::VectorStore =
        harness_canopy::rag::vector_store::VectorStore::new(dimensions).await?;
    let results: Vec<harness_canopy::rag::vector_store::SearchResult> =
        store.search_similar(&query_vec, 5).await?;

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
