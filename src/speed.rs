//! Service tiers are the running model's catalog, never a guessed list of speeds.
use crate::config::ModelInfo;
use serde_json::{Value, json};

pub(crate) const STANDARD: &str = "default";

pub(crate) fn model_tiers(model: &Value) -> Vec<Value> {
    let source = model
        .get("serviceTiers")
        .filter(|tiers| tiers.as_array().is_some_and(|tiers| !tiers.is_empty()))
        .or_else(|| model.get("additionalSpeedTiers"));
    source
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tier| {
            let id = tier.get("id").and_then(Value::as_str).or_else(|| tier.as_str())?;
            if id.is_empty() || id == STANDARD {
                return None;
            }
            let fallback = match id {
                "priority" | "fast" => "Fast",
                "ultrafast" => "Ultrafast",
                other => other,
            };
            Some(json!({"value": id, "name": tier.get("name").and_then(Value::as_str).unwrap_or(fallback),
            "description": tier.get("description").and_then(Value::as_str).unwrap_or("")}))
        })
        .collect()
}

pub(crate) fn option(model: &ModelInfo, picked: Option<&str>) -> Option<Value> {
    if model.service_tiers.is_empty() {
        return None;
    }
    let current = picked.or(model.default_service_tier.as_deref()).unwrap_or(STANDARD);
    let current = if current == STANDARD || model.service_tiers.iter().any(|tier| tier["value"] == current) {
        current
    } else {
        STANDARD
    };
    let mut tiers = vec![json!({"value": STANDARD, "name": "Standard"})];
    tiers.extend(model.service_tiers.iter().cloned());
    Some(json!({"id": "fast-mode", "name": "Speed", "category": "model_config", "type": "select",
        "currentValue": current, "options": tiers}))
}
