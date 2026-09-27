use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Openai,
    Gemini,
}

impl Route {
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Openai => "images/generations",
            Self::Gemini => "chat/completions",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ImageModel {
    pub model: String,
    pub canonical_id: String,
    pub account_ids: Vec<String>,
    pub owners: BTreeSet<String>,
    pub route: Option<Route>,
    pub metadata_match: String,
    pub evidence: String,
    pub descriptions: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Catalog {
    pub models: Vec<ImageModel>,
    pub warnings: Vec<String>,
}

#[derive(Deserialize)]
struct LiveModels {
    data: Vec<LiveModel>,
}

#[derive(Deserialize)]
struct LiveModel {
    id: String,
    #[serde(default)]
    owned_by: String,
}

pub fn compile(live: Value, metadata: Option<&Value>) -> Result<Catalog> {
    let live: LiveModels = serde_json::from_value(live).context("Invalid /models response")?;
    let mut index: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    if let Some(metadata) = metadata {
        for entries in metadata
            .as_object()
            .context("Invalid metadata catalog")?
            .values()
        {
            for entry in entries
                .as_array()
                .context("Invalid metadata catalog entries")?
            {
                if let Some(id) = entry.get("id").and_then(Value::as_str) {
                    index.entry(id).or_default().push(entry);
                }
            }
        }
    }
    let mut groups: BTreeMap<String, Vec<LiveModel>> = BTreeMap::new();
    for model in live.data {
        let canonical = model.id.rsplit('/').next().unwrap_or(&model.id).to_owned();
        groups.entry(canonical).or_default().push(model);
    }
    let mut models = Vec::new();
    for (canonical, group) in groups {
        let matches = index.get(canonical.as_str()).cloned().unwrap_or_default();
        let owners: BTreeSet<String> = group.iter().map(|item| item.owned_by.clone()).collect();
        let same_owner: Vec<&Value> = matches
            .iter()
            .copied()
            .filter(|entry| {
                entry
                    .get("owned_by")
                    .and_then(Value::as_str)
                    .is_some_and(|owner| owners.contains(owner))
            })
            .collect();
        let selected = if same_owner.is_empty() {
            &matches
        } else {
            &same_owner
        };
        let advertised = selected.iter().any(|entry| {
            entry
                .get("supportedOutputModalities")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item == "image"))
        });
        let lower = canonical.to_ascii_lowercase();
        let hinted = ["image", "imagen", "dall-e", "flux", "seedream"]
            .iter()
            .any(|hint| lower.contains(hint));
        if lower.starts_with("iq-") || (!advertised && !hinted) {
            continue;
        }
        let route = if canonical.starts_with("gpt-image-") {
            Some(Route::Openai)
        } else if canonical.starts_with("gemini-") && (advertised || lower.contains("image")) {
            Some(Route::Gemini)
        } else {
            None
        };
        let mut ids: Vec<String> = group.into_iter().map(|item| item.id).collect();
        ids.sort_by_key(|id| (id.contains('/'), id.clone()));
        ids.dedup();
        models.push(ImageModel {
            model: ids[0].clone(),
            canonical_id: canonical,
            account_ids: ids.into_iter().filter(|id| id.contains('/')).collect(),
            owners,
            route,
            metadata_match: if !same_owner.is_empty() {
                "same_owner"
            } else if !matches.is_empty() {
                "other_provider"
            } else {
                "missing"
            }
            .into(),
            evidence: if advertised {
                "image_output_modality"
            } else {
                "image_model_name"
            }
            .into(),
            descriptions: selected
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("description")
                        .or_else(|| entry.get("display_name"))
                        .and_then(Value::as_str)
                        .map(|text| text.chars().take(600).collect())
                })
                .collect(),
        });
    }
    Ok(Catalog {
        models,
        warnings: vec!["Listing does not guarantee quota. Missing metadata is unknown, not a quality ranking. Models with a null route are not supported by the generate tool.".into()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_missing_gpt_metadata_and_prefers_unprefixed_ids() {
        let catalog = compile(
            json!({"data": [
                {"id": "account/gpt-image-2", "owned_by": "openai"},
                {"id": "gpt-image-2", "owned_by": "openai"},
                {"id": "iq-image", "owned_by": "openai"}
            ]}),
            Some(&json!({})),
        )
        .unwrap();
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].model, "gpt-image-2");
        assert_eq!(catalog.models[0].route, Some(Route::Openai));
        assert_eq!(catalog.models[0].metadata_match, "missing");
    }

    #[test]
    fn input_images_do_not_imply_output_images() {
        let metadata = json!({"models": [
            {"id": "vision", "owned_by": "local", "supportedInputModalities": ["image"], "supportedOutputModalities": ["text"]},
            {"id": "vision", "owned_by": "other", "supportedOutputModalities": ["image"]},
            {"id": "gemini-test", "owned_by": "google", "supportedOutputModalities": ["image"]}
        ]});
        let catalog = compile(
            json!({"data": [
                {"id": "vision", "owned_by": "local"},
                {"id": "account/gemini-test", "owned_by": "google"}
            ]}),
            Some(&metadata),
        )
        .unwrap();
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].model, "account/gemini-test");
        assert_eq!(catalog.models[0].route, Some(Route::Gemini));
    }

    #[test]
    fn unknown_families_are_not_routed_as_gpt() {
        let catalog = compile(json!({"data": [{"id": "flux-test"}]}), None).unwrap();
        assert_eq!(catalog.models[0].route, None);
        assert!(compile(json!({}), None).is_err());
    }
}
