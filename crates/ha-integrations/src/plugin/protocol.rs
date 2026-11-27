//! NDJSON JSON-RPC 2.0 wire protocol between `hars` and out-of-process
//! plugins.
//!
//! Each message is one UTF-8 JSON object terminated by `\n`, bounded to
//! [`MAX_FRAME_BYTES`] (256 KiB) so a misbehaving plugin cannot exhaust heap
//! memory (`docs/PERF.md` rule 5).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Wire protocol version negotiated during `initialize`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum bytes accepted for a single NDJSON line from a plugin (256 KiB).
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Outgoing JSON-RPC 2.0 request from `hars` to a plugin.
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: Value,
}

impl<'a> RpcRequest<'a> {
    pub fn new(id: u64, method: &'a str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// Outgoing JSON-RPC 2.0 response from `hars` to a plugin request.
#[derive(Debug, Clone, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Any JSON-RPC 2.0 message received from a plugin: a response to a `hars`
/// request, a notification, or a request expecting a response.
#[derive(Debug, Clone, Deserialize)]
pub struct IncomingMessage {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// Parameters sent in `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub hars_version: String,
    pub domain: String,
    pub data_dir: String,
}

/// Result returned from `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub protocol_version: u32,
    #[serde(default)]
    pub plugin_version: Option<String>,
}

/// Parameters sent in `flow.step`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStepParams {
    pub flow_id: String,
    pub step_id: String,
    #[serde(default)]
    pub input: Option<Map<String, Value>>,
}

/// Field definition returned inside a plugin's config flow form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginField {
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub kind: PluginFieldKind,
    #[serde(default)]
    pub default: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginFieldKind {
    #[default]
    String,
    Select {
        /// `[value, label]` pairs.
        options: Vec<(String, String)>,
    },
}

/// Result returned from `flow.step`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FlowStepResult {
    Form {
        step_id: String,
        #[serde(default)]
        schema: Option<Vec<PluginField>>,
        #[serde(default)]
        errors: Option<Map<String, Value>>,
        #[serde(default)]
        description_placeholders: Option<Map<String, Value>>,
    },
    CreateEntry {
        title: String,
        #[serde(default)]
        unique_id: Option<String>,
        #[serde(default)]
        data: Map<String, Value>,
    },
    Abort {
        reason: String,
    },
}

/// Parameters sent in `entry.setup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntrySetupParams {
    pub entry_id: String,
    pub title: String,
    pub data: Map<String, Value>,
    #[serde(default)]
    pub options: Map<String, Value>,
}

/// Parameters sent in `entry.unload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryUnloadParams {
    pub entry_id: String,
}

/// Parameters sent in `entity.call` when a HA service targets a plugin entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityCallParams {
    pub entry_id: String,
    pub unique_id: String,
    pub domain: String,
    pub service: String,
    #[serde(default)]
    pub data: Map<String, Value>,
}

/// Parameters sent by a plugin in `device.upsert`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceUpsertParams {
    pub entry_id: String,
    /// Stable device identifier string within this plugin's domain.
    pub identifier: String,
    #[serde(default)]
    pub connections: Vec<(String, String)>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub sw_version: Option<String>,
    #[serde(default)]
    pub suggested_area: Option<String>,
}

/// Parameters sent by a plugin in `entity.upsert`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntityUpsertParams {
    pub entry_id: String,
    pub unique_id: String,
    /// Entity domain (`light`, `switch`, `button`, `scene`, `vacuum`,
    /// `select`, `number`, `cover`, `lock`, `fan`, `sensor`, `binary_sensor`).
    /// Must be listed in `plugin.json` capabilities.
    pub domain: String,
    /// Optional device identifier previously registered via `device.upsert`.
    #[serde(default)]
    pub device_identifier: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub object_id_base: Option<String>,
    #[serde(default)]
    pub original_icon: Option<String>,
    #[serde(default)]
    pub device_class: Option<String>,
    #[serde(default)]
    pub unit_of_measurement: Option<String>,
    #[serde(default)]
    pub entity_category: Option<String>,
    #[serde(default)]
    pub supported_features: u32,
    /// For `light` entities: supported color modes (`onoff`, `brightness`,
    /// `color_temp`, `hs`, `rgb`, `xy`, ...).
    #[serde(default)]
    pub supported_color_modes: Vec<String>,
    #[serde(default)]
    pub effect_list: Option<Vec<String>>,
    #[serde(default)]
    pub min_color_temp_kelvin: Option<i64>,
    #[serde(default)]
    pub max_color_temp_kelvin: Option<i64>,
    /// For `vacuum` entities: available fan speeds (`fan_speed_list`).
    #[serde(default)]
    pub fan_speed_list: Option<Vec<String>>,
    /// For `select` entities: selectable options (`options`).
    #[serde(default)]
    pub options: Option<Vec<String>>,
    /// For `number` entities: range and UI mode (`min`, `max`, `step`, `mode`).
    #[serde(default)]
    pub min_value: Option<f64>,
    #[serde(default)]
    pub max_value: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub mode: Option<String>,
    /// For `lock` entities: optional regex pattern validating `code`.
    #[serde(default)]
    pub code_format: Option<String>,
    /// For `fan` entities: available preset modes (`preset_modes`) and
    /// discrete speed count (`speed_count`, default 100).
    #[serde(default)]
    pub preset_modes: Option<Vec<String>>,
    #[serde(default)]
    pub speed_count: Option<u32>,
}

/// Parameters sent by a plugin in `state.set`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSetParams {
    pub entry_id: String,
    pub unique_id: String,
    pub state: String,
    #[serde(default)]
    pub attributes: Map<String, Value>,
}

/// Parameters sent by a plugin in `entry.status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryStatusParams {
    pub entry_id: String,
    /// `"loaded"`, `"setup_retry"`, or `"setup_error"`.
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Parameters sent by a plugin in `log`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogParams {
    pub level: String,
    pub message: String,
}

/// Read one `\n`-terminated UTF-8 line up to `max_bytes`, returning `Ok(None)`
/// on clean EOF and `Err(InvalidData)` if the frame exceeds `max_bytes`.
pub async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return String::from_utf8(buf)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if buf.len() + pos > max_bytes {
                reader.consume(pos + 1);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("plugin frame exceeded {max_bytes} bytes"),
                ));
            }
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return String::from_utf8(buf)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        let len = available.len();
        if buf.len() + len > max_bytes {
            reader.consume(len);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("plugin frame exceeded {max_bytes} bytes"),
            ));
        }
        buf.extend_from_slice(available);
        reader.consume(len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn reads_bounded_lines_and_rejects_oversized_frames() {
        let mut r = tokio::io::BufReader::new(Cursor::new(b"{\"ok\":1}\n{\"ok\":2}\r\n".to_vec()));
        assert_eq!(
            read_bounded_line(&mut r, 64).await.unwrap().as_deref(),
            Some(r#"{"ok":1}"#)
        );
        assert_eq!(
            read_bounded_line(&mut r, 64).await.unwrap().as_deref(),
            Some(r#"{"ok":2}"#)
        );
        assert_eq!(read_bounded_line(&mut r, 64).await.unwrap(), None);

        let huge = vec![b'a'; 128];
        let mut r = tokio::io::BufReader::new(Cursor::new(huge));
        let err = read_bounded_line(&mut r, 64).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
