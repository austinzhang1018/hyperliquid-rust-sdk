use crate::helpers::uuid_to_hex_string;

use super::{ order::OrderRequest, ClientOrderRequest };
use serde::{ Deserialize, Serialize };
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum ClientId {
    Oid(u64),
    Cloid(Uuid),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum SerializedClientId {
    U64(u64),
    Str(String),
}

impl From<ClientId> for SerializedClientId {
    fn from(client_id: ClientId) -> Self {
        match client_id {
            ClientId::Oid(oid) => SerializedClientId::U64(oid),
            ClientId::Cloid(cloid) => SerializedClientId::Str(uuid_to_hex_string(cloid)),
        }
    }
}
#[derive(Debug)]
pub struct ClientModifyRequest {
    pub oid: ClientId,
    pub order: ClientOrderRequest,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ModifyRequest {
    pub oid: SerializedClientId,
    pub order: OrderRequest,
}
