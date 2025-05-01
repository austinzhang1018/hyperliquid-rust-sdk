use crate::{
    exchange::{
        actions::{ BulkCancel, BulkCancelCloid, BulkModify, BulkOrder },
        cancel::{
            CancelRequest,
            CancelRequestCloid,
            ClientCancelRequest,
            ClientCancelRequestCloid,
        },
        exchange_client::Actions, // Reuse Actions enum
        modify::ModifyRequest, // Needed for potential future methods
        order::OrderRequest, // Needed for bulk_order
        ClientOrderRequest,
        ExchangeResponseStatus,
    },
    helpers::{ next_nonce, uuid_to_hex_string },
    info::info_client::InfoClient,
    meta::Meta,
    prelude::*,
    req::HttpClient, // Keep for potential meta fetching if needed, though InfoClient handles it
    signature::sign_l1_action,
    BaseUrl,
    Error,
};
use ethers::{ signers::{ LocalWallet, Signer }, types::{ Signature, H160, H256 } };
use futures_util::{ stream::SplitSink, SinkExt, StreamExt };
use log::{ debug, error, info, warn };
use serde::{ Deserialize, Serialize };
use serde_json::Value;
use std::{ collections::HashMap, sync::{ atomic::{ AtomicBool, Ordering }, Arc }, time::Duration };
use tokio::{ net::TcpStream, spawn, sync::{ mpsc, oneshot, Mutex }, time };
use tokio_tungstenite::{
    connect_async,
    tungstenite::{ self, protocol },
    MaybeTlsStream,
    WebSocketStream,
};

// --- WebSocket Request/Response Structs ---

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct WsActionRequestPayload {
    // #[serde(flatten)]
    action: Actions,
    nonce: u64,
    signature: Signature,
    vault_address: Option<H160>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct WsRequestPayload {
    #[serde(rename = "type")]
    req_type: String, // "action" or "info"
    payload: Value, // This will be the serialized WsActionRequestPayload or InfoRequest
}

#[derive(Serialize, Debug)]
struct WsPostRequest {
    method: String, // "post"
    id: u64,
    request: WsRequestPayload,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct WsResponsePayload {
    #[serde(rename = "type")]
    res_type: String, // "action", "info", "error"
    payload: Value, // This needs further deserialization based on res_type
}

#[derive(Deserialize, Debug, Clone)]
struct WsPostResponseData {
    id: u64,
    response: WsResponsePayload,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "channel", rename_all = "camelCase")]
enum WsMessage {
    Post {
        data: WsPostResponseData,
    },
    Pong,
    // Add other channel types if needed, e.g., SubscriptionResponse, Error
    #[serde(other)]
    Unknown, // Catch-all for channels we don't explicitly handle yet
}

#[derive(Serialize)]
struct Ping {
    method: &'static str,
}

// --- WsExchangeClient ---

#[derive(Debug)]
pub struct WsExchangeClient {
    writer: Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, protocol::Message>>>,
    stop_flag: Arc<AtomicBool>,
    _reader_handle: tokio::task::JoinHandle<()>,
    _ping_handle: tokio::task::JoinHandle<()>,
    pending_requests: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<ExchangeResponseStatus>>>>>,
    next_req_id: Arc<Mutex<u64>>,
    pub wallet: LocalWallet,
    pub meta: Meta,
    pub vault_address: Option<H160>,
    pub coin_to_asset: HashMap<String, u32>,
    is_mainnet: bool,
}

impl WsExchangeClient {
    const SEND_PING_INTERVAL: u64 = 50; // seconds

    pub async fn new(
        wallet: LocalWallet,
        base_url: Option<BaseUrl>,
        meta: Option<Meta>,
        vault_address: Option<H160>,
        reconnect: bool // Although reconnect logic is not fully implemented here yet
    ) -> Result<Self> {
        let base_url = base_url.unwrap_or(BaseUrl::Mainnet);
        let ws_url = format!("ws{}/ws", &base_url.get_url()[4..]);
        let is_mainnet = base_url.get_url() == BaseUrl::Mainnet.get_url();

        // Fetch metadata if not provided
        let meta = match meta {
            Some(m) => m,
            None => {
                let info = InfoClient::new(None, Some(base_url)).await?;
                info.meta().await?
            }
        };

        // Create coin_to_asset map
        let mut coin_to_asset = HashMap::new();
        for (asset_ind, asset) in meta.universe.iter().enumerate() {
            coin_to_asset.insert(asset.name.clone(), asset_ind as u32);
        }
        // Add spot assets if needed (requires InfoClient)
        // TODO: Consider if spot assets are needed for exchange actions via WS
        // let info = InfoClient::new(None, Some(base_url)).await?;
        // coin_to_asset = info.spot_meta().await?.add_pair_and_name_to_index_map(coin_to_asset);

        // Connect WebSocket
        let (ws_stream, _) = connect_async(&ws_url).await.map_err(|e|
            Error::Websocket(format!("Failed to connect: {}", e))
        )?;
        info!("WebSocket connected to {}", ws_url);
        let (writer, reader) = ws_stream.split();
        let writer = Arc::new(Mutex::new(writer));

        let stop_flag = Arc::new(AtomicBool::new(false));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let next_req_id = Arc::new(Mutex::new(1)); // Start IDs from 1

        // --- Reader Task ---
        let reader_stop_flag = stop_flag.clone();
        let reader_pending_requests = pending_requests.clone();
        let _reader_handle = spawn(async move {
            let mut reader = reader;
            while !reader_stop_flag.load(Ordering::Relaxed) {
                match reader.next().await {
                    Some(Ok(msg)) => {
                        if let Err(e) = Self::handle_message(msg, &reader_pending_requests).await {
                            error!("Error handling websocket message: {}", e);
                        }
                    }
                    Some(Err(e)) => {
                        error!("WebSocket read error: {}", e);
                        // TODO: Implement reconnect logic if `reconnect` is true
                        break; // Exit loop on error for now
                    }
                    None => {
                        warn!("WebSocket connection closed by server.");
                        // TODO: Implement reconnect logic if `reconnect` is true
                        break; // Exit loop on closure
                    }
                }
            }
            warn!("WebSocket reader task stopped.");
            // Notify pending requests about the disconnection
            let mut pending = reader_pending_requests.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(Error::Websocket("Connection closed".to_string())));
            }
        });

        // --- Ping Task ---
        let ping_stop_flag = stop_flag.clone();
        let ping_writer = writer.clone();
        let _ping_handle = spawn(async move {
            while !ping_stop_flag.load(Ordering::Relaxed) {
                match serde_json::to_string(&(Ping { method: "ping" })) {
                    Ok(payload) => {
                        let mut writer_guard = ping_writer.lock().await;
                        if let Err(err) = writer_guard.send(protocol::Message::Text(payload)).await {
                            error!("Error sending WebSocket ping: {}", err);
                            // If ping fails, the connection might be dead. Signal stop.
                            ping_stop_flag.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(err) => error!("Error serializing ping message: {}", err),
                }
                time::sleep(Duration::from_secs(Self::SEND_PING_INTERVAL)).await;
            }
            warn!("WebSocket ping task stopped.");
        });

        Ok(Self {
            writer,
            stop_flag,
            _reader_handle,
            _ping_handle,
            pending_requests,
            next_req_id,
            wallet,
            meta,
            vault_address,
            coin_to_asset,
            is_mainnet,
        })
    }

    async fn handle_message(
        msg: protocol::Message,
        pending_requests: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<ExchangeResponseStatus>>>>>
    ) -> Result<()> {
        match msg {
            protocol::Message::Text(text) => {
                debug!("Received WS message: {}", text);
                match serde_json::from_str::<WsMessage>(&text) {
                    Ok(WsMessage::Post { data }) => {
                        let mut pending = pending_requests.lock().await;
                        if let Some(sender) = pending.remove(&data.id) {
                            match data.response.res_type.as_str() {
                                "action" | "info" => {
                                    // Try to deserialize into the expected success structure
                                    match
                                        serde_json::from_value::<ExchangeResponseStatus>(
                                            data.response.payload
                                        )
                                    {
                                        Ok(status) => {
                                            if let Err(_) = sender.send(Ok(status)) {
                                                warn!(
                                                    "Receiver for request ID {} dropped.",
                                                    data.id
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "Failed to parse successful response payload for ID {}: {}",
                                                data.id,
                                                e
                                            );
                                            let _ = sender.send(
                                                Err(
                                                    Error::JsonParse(
                                                        format!("Failed to parse response payload: {}", e)
                                                    )
                                                )
                                            );
                                        }
                                    }
                                }
                                "error" => {
                                    // Handle error response
                                    let error_msg = data.response.payload.to_string(); // Or parse more specifically if structure is known
                                    warn!(
                                        "Received error response for request ID {}: {}",
                                        data.id,
                                        error_msg
                                    );
                                    let _ = sender.send(Err(Error::JsonParse(error_msg)));
                                }
                                _ => {
                                    warn!(
                                        "Received unknown response type '{}' for request ID {}",
                                        data.response.res_type,
                                        data.id
                                    );
                                    let _ = sender.send(
                                        Err(
                                            Error::JsonParse(
                                                format!(
                                                    "Unknown response type: {}",
                                                    data.response.res_type
                                                )
                                            )
                                        )
                                    );
                                }
                            }
                        } else {
                            warn!("Received response for unknown request ID: {}", data.id);
                        }
                    }
                    Ok(WsMessage::Pong) => {
                        debug!("Received Pong");
                    }
                    Ok(WsMessage::Unknown) => {
                        // Ignore channels we don't handle yet
                        debug!("Received unhandled WS message type: {}", text);
                    }
                    Err(e) => {
                        // Only log parsing errors if it looks like a JSON object, ignore plain text like "Websocket connection established."
                        if text.starts_with('{') || text.starts_with('[') {
                            error!("Failed to parse WebSocket message: {}. Text: {}", e, text);
                        } else {
                            info!("Received non-JSON WS message: {}", text);
                        }
                    }
                }
            }
            protocol::Message::Binary(_) => {
                warn!("Received unexpected binary message.");
            }
            protocol::Message::Ping(_) => {
                // tungstenite automatically handles sending Pongs for Pings
                debug!("Received Ping");
            }
            protocol::Message::Pong(_) => {
                // We send pings, server sends pongs
                debug!("Received Pong (in response to our Ping)");
            }
            protocol::Message::Close(_) => {
                info!("Received WebSocket close frame.");
            }
            protocol::Message::Frame(_) => {
                // Should not happen with default config
                warn!("Received unexpected frame.");
            }
        }
        Ok(())
    }

    async fn generate_req_id(&self) -> u64 {
        let mut guard = self.next_req_id.lock().await;
        let id = *guard;
        *guard += 1;
        id
    }

    async fn send_action_request(
        &self,
        action: Actions,
        wallet_override: Option<&LocalWallet>
    ) -> Result<ExchangeResponseStatus> {
        let wallet = wallet_override.unwrap_or(&self.wallet);
        let timestamp = next_nonce(); // Use as nonce

        // Hash and Sign
        let connection_id = action.hash(timestamp, self.vault_address)?;
        let signature = sign_l1_action(wallet, connection_id, self.is_mainnet)?;

        // Prepare Payload
        let action_payload_value = serde_json
            ::to_value(&action)
            .map_err(|e| Error::JsonParse(e.to_string()))?;

        let request_payload = WsRequestPayload {
            req_type: "action".to_string(),
            payload: action_payload_value, // The payload for "action" is just the action itself
        };

        let req_id = self.generate_req_id().await;

        // Wrap in Post Request
        let post_request = WsPostRequest {
            method: "post".to_string(),
            id: req_id,
            request: request_payload,
            // Note: The signature and nonce are part of the *inner* action payload for L1 actions,
            // but the documentation example shows them outside for the WS wrapper.
            // Let's re-evaluate based on the #attachment example for `action`.
            // The attachment shows the signature/nonce *inside* the `request.payload` for `type: "action"`.
            // Let's adjust the structure.
        };

        // --- Corrected Payload Structure based on Docs ---
        let action_payload_for_ws = WsActionRequestPayload {
            action, // The original Actions enum variant
            nonce: timestamp,
            signature,
            vault_address: self.vault_address,
        };
        let action_payload_value_for_ws = serde_json
            ::to_value(&action_payload_for_ws)
            .map_err(|e| Error::JsonParse(e.to_string()))?;

        let corrected_request_payload = WsRequestPayload {
            req_type: "action".to_string(),
            payload: action_payload_value_for_ws,
        };

        let corrected_post_request = WsPostRequest {
            method: "post".to_string(),
            id: req_id,
            request: corrected_request_payload,
        };
        // --- End Correction ---

        let request_json = serde_json
            ::to_string(&corrected_post_request)
            .map_err(|e| Error::JsonParse(e.to_string()))?;

        // Create oneshot channel for response
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(req_id, tx);
        }

        // Send request
        debug!("Sending WS request ID {}: {}", req_id, request_json);
        {
            let mut writer_guard = self.writer.lock().await;
            writer_guard
                .send(protocol::Message::Text(request_json)).await
                .map_err(|e| Error::Websocket(format!("Send error: {}", e)))?;
        }

        // Wait for response
        // TODO: Add timeout
        rx.await.map_err(|e| Error::Websocket(format!("Oneshot recv error: {}", e)))?
    }

    // --- Implemented Methods ---

    pub async fn bulk_order(
        &self,
        orders: Vec<ClientOrderRequest>,
        wallet: Option<&LocalWallet>
    ) -> Result<ExchangeResponseStatus> {
        let mut transformed_orders = Vec::new();
        for order in orders {
            transformed_orders.push(order.convert(&self.coin_to_asset)?);
        }

        let action = Actions::Order(BulkOrder {
            orders: transformed_orders,
            grouping: "na".to_string(),
            builder: None, // Builder not supported via WS post actions? Check docs. Assuming not for now.
        });

        self.send_action_request(action, wallet).await
    }

    pub async fn bulk_cancel(
        &self,
        cancels: Vec<ClientCancelRequest>,
        wallet: Option<&LocalWallet>
    ) -> Result<ExchangeResponseStatus> {
        let mut transformed_cancels = Vec::new();
        for cancel in cancels.into_iter() {
            let &asset = self.coin_to_asset.get(&cancel.asset).ok_or(Error::AssetNotFound)?;
            transformed_cancels.push(CancelRequest {
                asset,
                oid: cancel.oid,
            });
        }

        let action = Actions::Cancel(BulkCancel {
            cancels: transformed_cancels,
        });

        self.send_action_request(action, wallet).await
    }

    pub async fn bulk_cancel_by_cloid(
        &self,
        cancels: Vec<ClientCancelRequestCloid>,
        wallet: Option<&LocalWallet>
    ) -> Result<ExchangeResponseStatus> {
        let mut transformed_cancels: Vec<CancelRequestCloid> = Vec::new();
        for cancel in cancels.into_iter() {
            let &asset = self.coin_to_asset.get(&cancel.asset).ok_or(Error::AssetNotFound)?;
            transformed_cancels.push(CancelRequestCloid {
                asset,
                cloid: uuid_to_hex_string(cancel.cloid),
            });
        }

        let action = Actions::CancelByCloid(BulkCancelCloid {
            cancels: transformed_cancels,
        });

        self.send_action_request(action, wallet).await
    }

    // Add other methods (modify, leverage, etc.) here following the same pattern...
}

// --- Drop Implementation ---
impl Drop for WsExchangeClient {
    fn drop(&mut self) {
        info!("WsExchangeClient dropping. Signaling tasks to stop.");
        self.stop_flag.store(true, Ordering::Relaxed);
        // Note: Handles are not explicitly aborted here, they should exit gracefully when stop_flag is true.
    }
}
