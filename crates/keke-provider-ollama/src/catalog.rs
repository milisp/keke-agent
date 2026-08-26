//! Ollama's model catalog parsing.
//!
//! Ollama serves the OpenAI-compatible `/models` listing format.
//! The shared `keke_wire::WireClient::list_models` handles the parsing.
//! This module exists only for the custom description extraction from
//! Ollama-specific fields (parameter_size, quantization_level) if needed.

#[allow(dead_code)]
pub(crate) fn parse(body: &str) -> Result<Vec<keke_provider_api::ModelInfo>, serde_json::Error> {
    #[derive(serde::Deserialize)]
    struct OllamaModel {
        name: String,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        parameter_size: Option<String>,
        #[serde(default)]
        quantization_level: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct OllamaListing {
        models: Vec<OllamaModel>,
    }

    let listing: OllamaListing = serde_json::from_str(body)?;
    Ok(listing
        .models
        .into_iter()
        .map(|m| {
            let mut info = keke_provider_api::ModelInfo::new(m.name.clone());
            if let Some(name) = m.display_name {
                info.display_name = name;
            }
            let description = match (m.parameter_size, m.quantization_level) {
                (Some(size), Some(quant)) => Some(format!("{} {}", size, quant)),
                (Some(size), None) => Some(size),
                (None, Some(quant)) => Some(quant),
                (None, None) => None,
            };
            info.description = description;
            info
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ollama_listing() {
        let body = r#"{
            "models": [
                {"name": "llama3.1:8b", "display_name": "Llama 3.1 8B", "parameter_size": "8B", "quantization_level": "Q4_K_M"},
                {"name": "codellama:7b", "display_name": "Code Llama 7B", "parameter_size": "7B", "quantization_level": "Q4_K_M"}
            ]
        }"#;
        let models = parse(body).expect("parse");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama3.1:8b");
        assert_eq!(models[0].display_name, "Llama 3.1 8B");
        assert_eq!(models[0].description, Some("8B Q4_K_M".to_string()));
    }
}
