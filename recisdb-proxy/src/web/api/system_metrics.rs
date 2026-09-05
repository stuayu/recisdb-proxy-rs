use axum::{extract::State, Json};
use serde_json::json;
use std::sync::Arc;

use crate::web::state::WebState;

pub async fn get_system_metrics(State(web_state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let snapshot = web_state.system_metrics.snapshot().await;
    let Some(sample) = snapshot.latest else {
        return Json(json!({ "success": true, "current": null }));
    };
    Json(json!({
        "success": true,
        "timestamp": sample.timestamp,
        "cpu": {
            "usage_percent": sample.cpu_usage_percent,
            "cores": sample.cpu_cores,
            "load_average_1": sample.load_average_1,
            "load_average_5": sample.load_average_5,
            "load_average_15": sample.load_average_15,
        },
        "memory": {
            "used_bytes": sample.memory_used_bytes,
            "total_bytes": sample.memory_total_bytes,
        },
        "network": {
            "received_bytes": sample.network_received_bytes,
            "transmitted_bytes": sample.network_transmitted_bytes,
            "receive_bps": sample.network_receive_bps,
            "transmit_bps": sample.network_transmit_bps,
        },
        "gpus": sample.gpus,
    }))
}

pub async fn get_system_metrics_history(
    State(web_state): State<Arc<WebState>>,
) -> Json<serde_json::Value> {
    let history = web_state.system_metrics.snapshot().await.history;
    let series = |value: fn(&crate::metrics::system::SystemMetricSample) -> f64| {
        history
            .iter()
            .map(|sample| (sample.timestamp, value(sample)))
            .collect::<Vec<_>>()
    };
    let mut gpu_usage = std::collections::BTreeMap::<(String, usize), serde_json::Value>::new();
    for sample in &history {
        for gpu in &sample.gpus {
            let key = (format!("{:?}", gpu.vendor).to_lowercase(), gpu.index);
            let entry = gpu_usage.entry(key).or_insert_with(|| {
                json!({
                    "index": gpu.index,
                    "vendor": gpu.vendor,
                    "name": gpu.name,
                    "values": [],
                })
            });
            if let Some(values) = entry
                .get_mut("values")
                .and_then(serde_json::Value::as_array_mut)
            {
                if let Some(usage) = gpu.usage_percent {
                    values.push(json!([sample.timestamp, usage]));
                }
            }
        }
    }
    Json(json!({
        "success": true,
        "cpu_usage": series(|sample| sample.cpu_usage_percent as f64),
        "memory_used": series(|sample| sample.memory_used_bytes as f64),
        "network_receive_bps": series(|sample| sample.network_receive_bps),
        "network_transmit_bps": series(|sample| sample.network_transmit_bps),
        "gpu_usage": gpu_usage.into_values().collect::<Vec<_>>(),
    }))
}
