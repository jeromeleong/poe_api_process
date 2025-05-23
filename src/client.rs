use crate::types::*;
use crate::error::PoeError;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE};
use reqwest::Client;
use serde_json::Value;
use std::pin::Pin;
use futures_util::Stream;
use tracing::{debug, warn};

const BASE_URL: &str = "https://api.poe.com/bot/";
const POE_GQL_URL: &str = "https://poe.com/api/gql_POST";
const POE_GQL_MODEL_HASH: &str = "b24b2f2f6da147b3345eec1a433ed17b6e1332df97dea47622868f41078a40cc";

pub struct PoeClient {
    client: Client,
    bot_name: String,
    access_key: String,
}

impl PoeClient {
    pub fn new(bot_name: &str, access_key: &str) -> Self {
        debug!("建立新的 PoeClient 實例，bot_name: {}", bot_name);
        Self {
            client: Client::new(),
            bot_name: bot_name.to_string(),
            access_key: access_key.to_string(),
        }
    }

    pub async fn stream_request(&self, request: QueryRequest) -> Result<Pin<Box<dyn Stream<Item = Result<EventResponse, PoeError>> + Send>>, PoeError> {
        debug!("開始串流請求，bot_name: {}", self.bot_name);
        let url = format!("{}{}", BASE_URL, self.bot_name);
        
        debug!("發送請求至 URL: {}", url);
        let response = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", self.access_key))
            .json(&request)
            .send()
            .await?;
            
        if !response.status().is_success() {
            let status = response.status();
            warn!("API 請求失敗，狀態碼: {}", status);
            return Err(PoeError::BotError(format!("API 回應狀態碼: {}", status)));
        }

        debug!("成功接收到串流回應");
        let mut static_buffer = String::new();
        let mut current_event: Option<EventType> = None;
        let mut is_collecting_data = false;
        
        // 用於累積 tool_calls 的狀態
        let mut accumulated_tool_calls: Vec<AccumulatedToolCall> = Vec::new();
        let mut tool_calls_complete = false;

        let stream = response.bytes_stream().map(move |result| {
            result.map_err(PoeError::from).and_then(|chunk| {
                let chunk_str = String::from_utf8_lossy(&chunk);
                debug!("處理串流塊，大小: {} 字節", chunk.len());
                
                let mut events = Vec::new();
                // 將新的塊添加到靜態緩衝區
                static_buffer.push_str(&chunk_str);
                
                // 尋找完整的消息
                while let Some(newline_pos) = static_buffer.find('\n') {
                    let line = static_buffer[..newline_pos].trim().to_string();
                    static_buffer = static_buffer[newline_pos + 1..].to_string();
                    
                    if line.is_empty() { 
                        // 重置當前事件狀態，準備處理下一個事件
                        current_event = None;
                        is_collecting_data = false;
                        continue;
                    }
                    
                    if line == ": ping" {
                        debug!("收到 ping 訊號");
                        continue;
                    }
                    
                    if line.starts_with("event: ") {
                        let event_name = line.trim_start_matches("event: ").trim();
                        debug!("解析事件類型: {}", event_name);
                        
                        let event_type = match event_name {
                            "text" => {
                                EventType::Text
                            },
                            "replace_response" => {
                                EventType::ReplaceResponse
                            },
                            "json" => {
                                EventType::Json
                            },
                            "done" => {
                                EventType::Done
                            },
                            "error" => {
                                EventType::Error
                            },
                            _ => {
                                warn!("收到未知事件類型: {}", event_name);
                                continue;
                            }
                        };
                        current_event = Some(event_type);
                        is_collecting_data = false;
                        continue;
                    }
                    
                    if line.starts_with("data: ") {
                        let data = line.trim_start_matches("data: ").trim();
                        debug!("收到事件數據: {}", if data.len() > 100 { &data[..100] } else { data });
                        
                        if let Some(ref event_type) = current_event {
                            match event_type {
                                EventType::Text | EventType::ReplaceResponse => {
                                    if let Ok(json) = serde_json::from_str::<Value>(data) {
                                        if let Some(text) = json.get("text").and_then(Value::as_str) {
                                            debug!("解析到文本數據，長度: {}", text.len());
                                            events.push(Ok(EventResponse {
                                                event: event_type.clone(),
                                                data: Some(PartialResponse {
                                                    text: text.to_string(),
                                                }),
                                                error: None,
                                                tool_calls: None,
                                            }));
                                        }
                                    } else {
                                        debug!("JSON 解析失敗，可能是不完整的數據，等待更多數據");
                                        is_collecting_data = true;
                                    }
                                },
                                EventType::Json => {
                                    if let Ok(json) = serde_json::from_str::<Value>(data) {
                                        debug!("解析到 JSON 事件數據");
                                        
                                        // 檢查是否有 finish_reason: "tool_calls"，表示工具調用完成
                                        let finish_reason = json
                                            .get("choices")
                                            .and_then(|choices| choices.get(0))
                                            .and_then(|choice| choice.get("finish_reason"))
                                            .and_then(Value::as_str);
                                            
                                        if finish_reason == Some("tool_calls") {
                                            debug!("檢測到工具調用完成標誌");
                                            tool_calls_complete = true;
                                        }
                                        
                                        // 檢查是否包含 tool_calls delta
                                        let tool_calls_delta = json
                                            .get("choices")
                                            .and_then(|choices| choices.get(0))
                                            .and_then(|choice| choice.get("delta"))
                                            .and_then(|delta| delta.get("tool_calls"));
                                            
                                        if let Some(tool_calls_array) = tool_calls_delta {
                                            debug!("檢測到工具調用 delta");
                                            
                                            // 處理每個工具調用的 delta
                                            if let Some(tool_calls) = tool_calls_array.as_array() {
                                                for tool_call_delta in tool_calls {
                                                    let index = tool_call_delta
                                                        .get("index")
                                                        .and_then(Value::as_u64)
                                                        .unwrap_or(0) as usize;
                                                        
                                                    // 確保 accumulated_tool_calls 有足夠的元素
                                                    while accumulated_tool_calls.len() <= index {
                                                        accumulated_tool_calls.push(AccumulatedToolCall::default());
                                                    }
                                                    
                                                    // 更新 id 和 type
                                                    if let Some(id) = tool_call_delta.get("id").and_then(Value::as_str) {
                                                        accumulated_tool_calls[index].id = id.to_string();
                                                    }
                                                    
                                                    if let Some(type_str) = tool_call_delta.get("type").and_then(Value::as_str) {
                                                        accumulated_tool_calls[index].r#type = type_str.to_string();
                                                    }
                                                    
                                                    // 更新 function 相關欄位
                                                    if let Some(function) = tool_call_delta.get("function") {
                                                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                                                            accumulated_tool_calls[index].function_name = name.to_string();
                                                        }
                                                        
                                                        if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                                                            accumulated_tool_calls[index].function_arguments.push_str(args);
                                                        }
                                                    }
                                                }
                                            }
                                        } else if !tool_calls_complete {
                                            // 如果沒有 tool_calls delta 且工具調用尚未完成，
                                            // 則按一般 JSON 處理（例如 chat.completion.chunk 的文本部分）
                                            // 避免在工具調用完成後，將 finish_reason 事件誤判為普通 JSON
                                            events.push(Ok(EventResponse {
                                                event: EventType::Json,
                                                data: Some(PartialResponse {
                                                    text: data.to_string(),
                                                }),
                                                error: None,
                                                tool_calls: None,
                                            }));
                                        }
                                    } else {
                                        debug!("JSON 事件解析失敗，可能是不完整的數據");
                                        is_collecting_data = true;
                                    }
                                },
                                EventType::Done => {
                                    debug!("收到完成事件");
                                    events.push(Ok(EventResponse {
                                        event: EventType::Done,
                                        data: None,
                                        error: None,
                                        tool_calls: None,
                                    }));
                                    current_event = None;
                                },
                                EventType::Error => {
                                    if let Ok(json) = serde_json::from_str::<Value>(data) {
                                        let text = json.get("text")
                                            .and_then(Value::as_str)
                                            .unwrap_or("未知錯誤");
                                        let allow_retry = json.get("allow_retry")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false);
                                            
                                        warn!("收到錯誤事件: {}, 可重試: {}", text, allow_retry);
                                        events.push(Ok(EventResponse {
                                            event: EventType::Error,
                                            data: None,
                                            error: Some(ErrorResponse {
                                                text: text.to_string(),
                                                allow_retry,
                                            }),
                                            tool_calls: None,
                                        }));
                                    } else {
                                        warn!("無法解析錯誤事件數據: {}", data);
                                    }
                                    current_event = None;
                                }
                            }
                        } else {
                            debug!("收到數據但沒有當前事件類型");
                        }
                    } else if is_collecting_data {
                        // 嘗試解析累積的 JSON
                        debug!("嘗試解析未完整的 JSON 數據: {}", line);
                        if let Some(ref event_type) = current_event {
                            match event_type {
                                EventType::Text | EventType::ReplaceResponse => {
                                    if let Ok(json) = serde_json::from_str::<Value>(&line) {
                                        if let Some(text) = json.get("text").and_then(Value::as_str) {
                                            debug!("成功解析到累積的 JSON 文本，長度: {}", text.len());
                                            events.push(Ok(EventResponse {
                                                event: event_type.clone(),
                                                data: Some(PartialResponse {
                                                    text: text.to_string(),
                                                }),
                                                error: None,
                                                tool_calls: None,
                                            }));
                                            is_collecting_data = false;
                                            current_event = None;
                                        }
                                    }
                                },
                                EventType::Json => {
                                    if let Ok(json) = serde_json::from_str::<Value>(&line) {
                                        debug!("成功解析到累積的 JSON 事件數據");
                                        
                                        // 檢查是否有 finish_reason: "tool_calls"
                                        let finish_reason = json
                                            .get("choices")
                                            .and_then(|choices| choices.get(0))
                                            .and_then(|choice| choice.get("finish_reason"))
                                            .and_then(Value::as_str);
                                            
                                        if finish_reason == Some("tool_calls") {
                                            debug!("檢測到工具調用完成標誌");
                                            tool_calls_complete = true;
                                        }
                                        
                                        // 檢查是否包含 tool_calls delta
                                        let tool_calls_delta = json
                                            .get("choices")
                                            .and_then(|choices| choices.get(0))
                                            .and_then(|choice| choice.get("delta"))
                                            .and_then(|delta| delta.get("tool_calls"));
                                            
                                        if let Some(tool_calls_array) = tool_calls_delta {
                                            debug!("檢測到工具調用 delta");
                                            
                                            // 處理每個工具調用的 delta
                                            if let Some(tool_calls) = tool_calls_array.as_array() {
                                                for tool_call_delta in tool_calls {
                                                    let index = tool_call_delta
                                                        .get("index")
                                                        .and_then(Value::as_u64)
                                                        .unwrap_or(0) as usize;
                                                        
                                                    // 確保 accumulated_tool_calls 有足夠的元素
                                                    while accumulated_tool_calls.len() <= index {
                                                        accumulated_tool_calls.push(AccumulatedToolCall::default());
                                                    }
                                                    
                                                    // 更新 id 和 type
                                                    if let Some(id) = tool_call_delta.get("id").and_then(Value::as_str) {
                                                        accumulated_tool_calls[index].id = id.to_string();
                                                    }
                                                    
                                                    if let Some(type_str) = tool_call_delta.get("type").and_then(Value::as_str) {
                                                        accumulated_tool_calls[index].r#type = type_str.to_string();
                                                    }
                                                    
                                                    // 更新 function 相關欄位
                                                    if let Some(function) = tool_call_delta.get("function") {
                                                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                                                            accumulated_tool_calls[index].function_name = name.to_string();
                                                        }
                                                        
                                                        if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                                                            accumulated_tool_calls[index].function_arguments.push_str(args);
                                                        }
                                                    }
                                                }
                                            }
                                            
                                            // 如果工具調用完成，則創建並發送 EventResponse
                                            if tool_calls_complete && !accumulated_tool_calls.is_empty() {
                                                let complete_tool_calls = accumulated_tool_calls
                                                    .iter()
                                                    .filter(|tc| !tc.id.is_empty() && !tc.function_name.is_empty())
                                                    .map(|tc| ToolCall {
                                                        id: tc.id.clone(),
                                                        r#type: tc.r#type.clone(),
                                                        function: ToolCallFunction {
                                                            name: tc.function_name.clone(),
                                                            arguments: tc.function_arguments.clone(),
                                                        },
                                                    })
                                                    .collect::<Vec<ToolCall>>();
                                                    
                                                if !complete_tool_calls.is_empty() {
                                                    debug!("發送完整的工具調用，數量: {}", complete_tool_calls.len());
                                                    events.push(Ok(EventResponse {
                                                        event: EventType::Json,
                                                        data: None,
                                                        error: None,
                                                        tool_calls: Some(complete_tool_calls),
                                                    }));
                                                    
                                                    // 重置累積狀態
                                                    accumulated_tool_calls.clear();
                                                    tool_calls_complete = false;
                                                }
                                            }
                                        } else {
                                            // 如果沒有 tool_calls delta，則按一般 JSON 處理
                                            events.push(Ok(EventResponse {
                                                event: EventType::Json,
                                                data: Some(PartialResponse {
                                                    text: line.to_string(),
                                                }),
                                                error: None,
                                                tool_calls: None,
                                            }));
                                        }
                                        is_collecting_data = false;
                                        current_event = None;
                                    }
                                },
                                EventType::Done | EventType::Error => {
                                    // 這些事件類型不應該有累積的數據
                                    is_collecting_data = false;
                                }
                            }
                        }
                    }
                }
                
                // 在處理完 chunk 中的所有行之後，檢查是否需要發送最終的 tool_calls 事件
                if tool_calls_complete && !accumulated_tool_calls.is_empty() {
                    let complete_tool_calls = accumulated_tool_calls
                        .iter()
                        .filter(|tc| !tc.id.is_empty() && !tc.function_name.is_empty())
                        .map(|tc| ToolCall {
                            id: tc.id.clone(),
                            r#type: tc.r#type.clone(),
                            function: ToolCallFunction {
                                name: tc.function_name.clone(),
                                arguments: tc.function_arguments.clone(),
                            },
                        })
                        .collect::<Vec<ToolCall>>();
                        
                    if !complete_tool_calls.is_empty() {
                        debug!("發送最終的完整工具調用，數量: {}", complete_tool_calls.len());
                        events.push(Ok(EventResponse {
                            event: EventType::Json, // 仍然是 Json 事件，但包含完整的 tool_calls
                            data: None,
                            error: None,
                            tool_calls: Some(complete_tool_calls),
                        }));
                        
                        // 重置狀態
                        accumulated_tool_calls.clear();
                        tool_calls_complete = false;
                    }
                }
                
                Ok(events)
            })
        })
        .flat_map(|result| {
            futures_util::stream::iter(match result {
                Ok(events) => events,
                Err(e) => {
                    warn!("串流處理錯誤: {}", e);
                    vec![Err(e)]
                },
            })
        });

        Ok(Box::pin(stream))
    }

    pub async fn send_tool_results(
        &self,
        original_request: QueryRequest,
        tool_calls: Vec<ToolCall>,
        tool_results: Vec<ToolResult>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<EventResponse, PoeError>> + Send>>, PoeError> {
        debug!("發送工具調用結果，bot_name: {}", self.bot_name);
        
        // 創建包含工具結果的新請求
        let mut request = original_request;
        request.tool_calls = Some(tool_calls);
        request.tool_results = Some(tool_results);
        
        // 發送請求並處理響應
        self.stream_request(request).await
    }
}

pub async fn get_model_list(language_code: Option<&str>) -> Result<ModelListResponse, PoeError> {
    debug!("開始獲取模型列表，語言代碼: {:?}", language_code);
    
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| {
            warn!("建立 HTTP 客戶端失敗: {}", e);
            PoeError::BotError(e.to_string())
        })?;

    let payload = serde_json::json!({
        "queryName": "ExploreBotsListPaginationQuery",
        "variables": {
            "categoryName": "defaultCategory",
            "count": 150
        },
        "extensions": {
            "hash": POE_GQL_MODEL_HASH
        }
    });

    debug!("準備 GraphQL 請求載荷，使用 hash: {}", POE_GQL_MODEL_HASH);
    
    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Accept-Language", HeaderValue::from_static("zh-TW,zh;q=0.9,en-US;q=0.8,en;q=0.7"));
    headers.insert("Origin", HeaderValue::from_static("https://poe.com"));
    headers.insert("Referer", HeaderValue::from_static("https://poe.com"));
    headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
    headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
    headers.insert("Sec-Fetch-Site", HeaderValue::from_static("same-origin"));
    headers.insert("poegraphql", HeaderValue::from_static("1"));
    
    if let Some(code) = language_code {
        let cookie_value = format!("Poe-Language-Code={}; p-b=1", code);
        debug!("設置語言 Cookie: {}", cookie_value);
        
        headers.insert(COOKIE, HeaderValue::from_str(&cookie_value)
            .map_err(|e| {
                warn!("設置 Cookie 失敗: {}", e);
                PoeError::BotError(e.to_string())
            })?);
    }

    debug!("發送 GraphQL 請求至 {}", POE_GQL_URL);
    
    let response = client.post(POE_GQL_URL)
        .headers(headers)
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            warn!("發送 GraphQL 請求失敗: {}", e);
            PoeError::RequestFailed(e)
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_else(|_| "無法讀取回應內容".to_string());
        warn!("GraphQL API 回應錯誤 - 狀態碼: {}, 內容: {}", status, text);
        return Err(PoeError::BotError(format!("API 回應錯誤 - 狀態碼: {}, 內容: {}", status, text)));
    }

    debug!("成功接收到 GraphQL 回應");
    
    let json_value = response.text().await
        .map_err(|e| {
            warn!("讀取 GraphQL 回應內容失敗: {}", e);
            PoeError::RequestFailed(e)
        })?;

    let data: Value = serde_json::from_str(&json_value)
        .map_err(|e| {
            warn!("解析 GraphQL 回應 JSON 失敗: {}", e);
            PoeError::JsonParseFailed(e)
        })?;

    let mut model_list = Vec::with_capacity(150);
    
    if let Some(edges) = data["data"]["exploreBotsConnection"]["edges"].as_array() {
        debug!("找到 {} 個模型節點", edges.len());
        
        for edge in edges {
            if let Some(handle) = edge["node"]["handle"].as_str() {
                debug!("解析模型 ID: {}", handle);
                model_list.push(ModelInfo {
                    id: handle.to_string(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "poe".to_string(),
                });
            } else {
                debug!("模型節點中找不到 handle 欄位");
            }
        }
    } else {
        warn!("無法從回應中取得模型列表節點");
        return Err(PoeError::BotError("無法從回應中取得模型列表".to_string()));
    }

    if model_list.is_empty() {
        warn!("取得的模型列表為空");
        return Err(PoeError::BotError("取得的模型列表為空".to_string()));
    }

    debug!("成功解析 {} 個模型", model_list.len());
    Ok(ModelListResponse { data: model_list })
}