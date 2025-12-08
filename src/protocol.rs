use color_eyre::eyre::Error;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PortId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Port {
    pub id: PortId,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct InitializeRequest {
    pub version: u8,
    pub client_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct InitializeResponse {
    pub ports: Vec<Port>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConnectRequest {
    pub id: PortId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConnectResponse {
    pub udp_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "command")]
pub enum EstablishedCommand {
    ShutdownWithoutStop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Ack {
    pub ack: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ErrorResponse {
    pub error: String,
    pub context: Vec<String>,
    pub ansi: String,
}

impl ErrorResponse {
    pub fn of_report(report: &Error) -> Self {
        let mut context = vec![];
        for entry in report.chain().skip(1) {
            context.push(entry.to_string());
        }

        Self {
            error: report.to_string(),
            context,
            ansi: format!("{:?}", report),
        }
    }
}
