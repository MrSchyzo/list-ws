use std::{collections::HashMap, future::IntoFuture, sync::Arc};

use tokio::{net::{TcpListener, TcpStream}, sync::{broadcast::{self, Receiver, Sender}, RwLock}, task::JoinError};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{handshake::server::{Request, Response}, Message},
};
#[macro_use]
extern crate log;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), JoinError> {
    env_logger::builder().format_timestamp(None).init();

    tokio::spawn(async move { server().await; }).into_future().await
}

struct ApplicationCore {
    root_broadcast: Sender<Arc<OutboundPayload>>,
    broadcasts: HashMap<Uuid, Arc<Sender<Arc<OutboundPayload>>>>,
    state: HashMap<Uuid, (String, HashMap<Uuid, (String, bool)>)>
}

impl ApplicationCore {

    pub fn new() -> Self {
        Self {
            root_broadcast: broadcast::channel(16).0,
            broadcasts: HashMap::new(),
            state: HashMap::new(),
        }
    }

    pub fn get_lists(&self) -> (Receiver<Arc<OutboundPayload>>, Vec<List>) {
        (
            self.root_broadcast.subscribe(), 
            self.state.iter().map(|(id, (name, _))| List{ id: id.to_string(), name: name.to_owned() }).collect()
        )
    }

    pub fn new_list(&mut self, name: String) {
        let id = generate_while(Uuid::new_v4, |id| self.state.contains_key(id));
        let str_id = id.to_string();
        self.state.insert(id.clone(), (name.clone(), HashMap::new()));
        //TODO: what to do with send failure?
        let _ = self.root_broadcast.send(
            Arc::new(OutboundPayload::ListsAppear { incoming: vec![List {id: str_id, name: name}] })
        ); 
        self.broadcasts.insert(id, Arc::new(broadcast::channel(16).0));
    }

    pub fn show_list(&self, id: &Uuid) -> Option<(Receiver<Arc<OutboundPayload>>, OutboundPayload)> {
        let receiver = self.broadcasts.get(id).map(|sender| sender.subscribe());
        let response = self.state
            .get(id)
            .map(|(name, items)| {
                OutboundPayload::ListIsShown { 
                    list_id: name.clone(), 
                    items: items.iter()
                        .map(|(id, (name, checked))| {
                            ListItem {
                                id: id.to_string(), 
                                name: name.clone(), 
                                is_checked: checked.to_owned()
                            }
                        }).collect()
                }
            });

        receiver.zip(response)
    }

    pub fn delete_list(&mut self, id: &Uuid) {
        self.state.remove(id);
        let _ = self.root_broadcast.send(
            Arc::new(OutboundPayload::ListsDisappear { outgoing: vec![id.to_string()]})
        );
        if let Some(broadcast) = self.broadcasts.remove(id) {
            let _ = broadcast.send(Arc::new(OutboundPayload::ListsAreShown { lists: 
                self.state.iter().map(|(id, (name, _))| List{id: id.to_string(), name: name.clone()}).collect()
            }));
        }
    }

    pub fn add_item(&mut self, list_id: &Uuid, item_name: String) -> bool {
        let list = match self.state.get_mut(list_id) {
            Some((_, items)) => items,
            None => return false,
        };
        let item_id = generate_while(Uuid::new_v4, |id| list.contains_key(id));
        list.insert(item_id, (item_name.clone(), false));
        if let Some(broadcast) = self.broadcasts.get(list_id) {
            let _ = broadcast.send(Arc::new(
                OutboundPayload::ListItemsAppear { 
                    list_id: list_id.to_string(), 
                    incoming: vec![ListItem {
                        id: item_id.to_string(), 
                        name: item_name, 
                        is_checked: false
                    }] 
                })
            );
        }
        true
    }

    pub fn remove_item(&mut self, list_id: &Uuid, item_id: &Uuid) -> bool {
        if let Some(broadcast) = 
            self.state.get_mut(list_id)
                .and_then(|(_, list)| list.remove(item_id))
                .and_then(|_| self.broadcasts.get(list_id)) {
            let _ = broadcast.send(Arc::new(
                OutboundPayload::ListItemsDisappear { 
                    list_id: list_id.to_string(), 
                    outgoing: vec![item_id.to_string()] 
                })
            );
            true
        } else {
            false
        }
    }

    pub fn update_item(&mut self, list_id: &Uuid, item_id: &Uuid, checked: bool) -> bool {
        let list = match self.state.get_mut(list_id) {
            Some((_, items)) => items,
            None => return false,
        };

        match list.get_mut(item_id) {
            Some(pair) => pair,
            None => return false,
        }.1 = checked;

        if let Some(broadcast) = self.broadcasts.get(list_id) {
            let _ = broadcast.send(Arc::new(
                OutboundPayload::ListItemsUpdate { 
                    list_id: list_id.to_string(), 
                    is_checked: list.iter().to_owned().map(|(id, (_, checked))| (id.to_string(), *checked)).collect() 
                })
            );
        }
        true
    }
}

pub fn generate_while<
    T, 
    Generator: Fn() -> T, 
    Validator: Fn(&T) -> bool
>(generate: Generator, guard: Validator) -> T {
    loop {
        let candidate = generate();
        if guard(&candidate) {
            continue;
        } else {
            break candidate;
        }
    }
}

async fn server() {
    let server = TcpListener::bind("127.0.0.1:8080").await.unwrap();
    let app = Arc::new(RwLock::new(ApplicationCore::new()));

    while let Ok((stream, _)) = server.accept().await {
        tokio::spawn(accept_connection(stream, app.clone()));
    }
}


async fn accept_connection(stream: TcpStream, app: Arc<RwLock<ApplicationCore>>) {
    let callback = |req: &Request, mut response: Response| {
        info!("Received a new ws handshake");
        info!("The request's path is: {}", req.uri().path());
        info!("The request's headers are:");
        for (ref header, _value) in req.headers() {
            info!("* {}: {:?}", header, _value);
        }

        let headers = response.headers_mut();
        headers.append("MyCustomHeader", ":)".parse().unwrap());

        Ok(response)
    };
    let mut ws_stream = accept_hdr_async(stream, callback)
        .await
        .expect("Error during the websocket handshake occurred")
        .peekable();

    let msg = ws_stream.next().await.unwrap();
    {
        let input: Inbound = serde_json::from_str(msg.unwrap().into_text().unwrap().as_str()).unwrap();
        let output = match input {
            Inbound::Authenticate { .. } => Outbound::Ok { response: OutboundPayload::NoContent {} },
            _ => Outbound::UserError { error_message: "Expecting authentication payload".to_owned() }
        };
        let _ = ws_stream.send(Message::Text(serde_json::to_string(&output).unwrap())).await;
    }

    let (channel, lists) = { app.read().await.get_lists() };
    let mut state = UserState::ShowLists { channel };
    let update = OutboundPayload::ListsAreShown { lists };
    let _ = ws_stream.send(Message::Text(serde_json::to_string(&update).unwrap())).await;

    loop {
        let input = if let Some(channel) = state.get_channel() {
            tokio::select! {
                output = channel.recv() => {
                    let message = output.unwrap();
                    ws_stream.send(Message::Text(serde_json::to_string(message.as_ref()).unwrap())).await.unwrap();
                    continue;
                }
                input = ws_stream.next() => {
                    input
                },
            }
        } else {
            ws_stream.next().await
        };
        let payload: Inbound = serde_json::from_str(input.unwrap().unwrap().into_text().unwrap().as_str()).unwrap();
        
        info!("Incoming {state:?}, {payload:?}");

        let (output, new_state) = match (state, payload) {
            (state @ UserState::SingleList { .. }, Inbound::AddItem { list_id, name }) => {
                { app.write().await.add_item(&list_id, name) };
                (
                    Outbound::Ok { response: OutboundPayload::NoContent {} },
                    state
                )
            },
            (state @ UserState::SingleList { .. }, Inbound::UpdateItem { list_id, item_id, operation: ItemOp::Remove }) => {
                { app.write().await.remove_item(&list_id, &item_id) };
                (
                    Outbound::Ok { response: OutboundPayload::NoContent {} },
                    state
                )
            },
            (state @ UserState::SingleList { .. }, Inbound::UpdateItem {item_id, list_id, operation}) => {
                { app.write().await.update_item(&list_id, &item_id, matches!(operation, ItemOp::Check)) };
                (
                    Outbound::Ok { response: OutboundPayload::NoContent {} },
                    state
                )
            },
            (state @ UserState::SingleList { .. }, Inbound::DeleteList { id_to_remove: id }) => {
                { app.write().await.delete_list(&id) };
                (
                    Outbound::Ok { response: OutboundPayload::NoContent {} },
                    state
                )
            },
            (state @ UserState::ShowLists { .. }, Inbound::NewList { name }) => {
                { app.write().await.new_list(name) };
                (
                    Outbound::Ok { response: OutboundPayload::NoContent {} },
                    state
                )
            },
            (state @ UserState::ShowLists { .. }, Inbound::DeleteList { id_to_remove: id }) => {
                { app.write().await.delete_list(&id) };
                (
                    Outbound::Ok { response: OutboundPayload::NoContent {} },
                    state
                )
            },
            (state @ UserState::ShowLists { .. }, Inbound::ShowList { id }) => {
                match { app.read().await.show_list(&id) } {
                    None => (
                        Outbound::UserError { error_message: format!("{id} not found") },
                        state
                    ),
                    Some((channel, output)) => (
                        Outbound::Ok { response: output },
                        UserState::SingleList { channel }
                    ),
                }
            },
            (_, Inbound::ShowLists { }) => {
                let (channel, lists) = { app.read().await.get_lists() };
                (
                    Outbound::Ok { response: OutboundPayload::ListsAreShown { lists } },
                    UserState::ShowLists { channel }
                )
            },
            (state, _) => {
                (
                    Outbound::SystemError { error: "Unexpected state".to_string() },
                    state
                )
            }
        };

        state = new_state;
        let _ = ws_stream.send(Message::Text(serde_json::to_string(&output).unwrap())).await;
    }
}

#[derive(Debug)]
pub enum UserState {
    ShowLists {
        channel: Receiver<Arc<OutboundPayload>>
    },
    SingleList {
        channel: Receiver<Arc<OutboundPayload>>
    }
}

impl UserState {
    pub fn get_channel(&mut self) -> Option<&mut Receiver<Arc<OutboundPayload>>> {
        match self {
            UserState::ShowLists { channel } 
            | UserState::SingleList { channel } => Some(channel),
        }
    }
}

pub enum UserScreen {
    ShowLists,
    SingleList,
}

pub struct UserInfo {
    pub id: String,
}


#[derive(Deserialize)]
#[serde(untagged)]
#[derive(Debug)]
enum Inbound {
    Authenticate { user: String, password: String },
    DeleteList { id_to_remove: Uuid },
    ShowList { id: Uuid },
    UpdateItem { list_id: Uuid, item_id: Uuid, operation: ItemOp },
    AddItem { list_id: Uuid, name: String, },
    NewList { name: String },
    ShowLists { },
}

#[derive(Deserialize)]
#[derive(Debug)]
enum ItemOp {
    Uncheck,
    Check,
    Remove,
}


#[derive(Serialize)]
#[serde(untagged)]
enum Outbound {
    SystemError { error: String },
    UserError { error_message: String },
    Ok { response: OutboundPayload }
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum OutboundPayload {
    NoContent {},
    ListsAreShown { lists: Vec<List> },
    ListsAppear { incoming: Vec<List> },
    ListsDisappear { outgoing: Vec<String>, },
    ListIsShown { list_id: String, items: Vec<ListItem> },
    ListItemsAppear { list_id: String, incoming: Vec<ListItem> },
    ListItemsDisappear { list_id: String, outgoing: Vec<String> },
    ListItemsUpdate { list_id: String, is_checked: HashMap<String, bool> },
}

#[derive(Serialize)]
pub struct List {
    pub id: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct ListItem {
    pub id: String,
    pub name: String,
    pub is_checked: bool,
}