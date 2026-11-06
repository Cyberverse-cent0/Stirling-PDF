use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HealthResponse {
    pub status: String,
    pub smart_model: String,
    pub fast_model: String,
}
