use {
    rengo_common::{
        networking::*,
        logic::*,
    },
    anyhow::Result,
    log::{info, warn, debug, error, trace},
    std::{
        thread,
        net,
        sync::Arc,
        collections::{
            HashMap,
        },
    },
    futures::{
        future,
        channel::mpsc,
        lock::Mutex,
        StreamExt,
        SinkExt,
    },
    smol::{Timer, Task, Async},
    tungstenite::Message as WebSocketMessage,
    async_tungstenite::WebSocketStream,
};

pub enum ThreadMessage {
    Terminate,
}

pub enum RoomCommand {
    PlayerQuery(mpsc::UnboundedSender<RoomCommand>, String),
    PlayerQueryResponse(bool),
    PlayerNumQuery(mpsc::UnboundedSender<RoomCommand>),
    PlayerNumQueryResponse(u8),
    FullQuery(mpsc::UnboundedSender<RoomCommand>),
    FullQueryResponse(bool),
    PlayerAdd(String, mpsc::UnboundedSender<ServerMessage>),
    PlayerRemove(String),
}

pub enum RoomMessage {
    ClientMessage(String, ClientMessage),
    RoomCommand(RoomCommand),
}

pub type Thread = (thread::JoinHandle<()>, mpsc::UnboundedSender<ThreadMessage>);
pub type House = Arc<Mutex<HashMap<String, RoomSender>>>;
pub type RoomSender = mpsc::UnboundedSender::<RoomMessage>;
pub type RoomReceiver = mpsc::UnboundedReceiver::<RoomMessage>;

pub struct ServerRoom {
    capacity: u8,
    board: Board,
    current_player: String,
    players: Vec<(String, (mpsc::UnboundedSender::<ServerMessage>, Player, bool))>,
}

pub fn spawn_smol_thread(name: &str) -> std::io::Result<Thread> {
    let name = name.to_owned();
    let (tx, mut rx) = mpsc::unbounded();
    Ok((thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            smol::run(async move {
                loop {
                    match rx.next().await {
                        Some(thread_message) => match thread_message {
                            ThreadMessage::Terminate => {
                                debug!("Thread `{}` has terminated", name);
                                break;
                            }
                        } 

                        None => {
                            warn!("Thread `{}`: Thread command queue has closed unexpectedly.", name);
                            break;
                        }
                    }
                }
            })
        })?,
        tx))
}

pub fn handle_console_input(mut threadpool: Vec<Thread>) {
    let reader = std::io::stdin();
    loop {
        let mut line = String::new();
        if let Err(e) = reader.read_line(&mut line) {
            error!("Unable to read from console: {}", e);
            continue;
        }

        line = line.trim_end().to_owned();

        let words: Vec<&str> = line.split(' ').collect();

        match words.iter().next() {
            Some(command) => {
                match &command.to_lowercase()[..] {
                    "stop" => {
                        info!("[Console]: Shutting down");
                        while let Some(mut thread) = threadpool.pop() {
                            smol::run(async move {
                                if let Err(e) = thread.1.send(ThreadMessage::Terminate).await {
                                    error!("[Console]: Unable to shutdown thread: {}", e);
                                }

                                if let Err(_) = thread.0.join() {
                                    error!("[Console]: Unable to join thread");
                                }
                            });
                        }

                        std::process::exit(0);
                    }
                    _ => {
                        error!("[Console]: Invalid Command");
                    }
                }
            }
            None => {
                error!("[Console]: Enter a command");
            }
        }
    }
}

async fn spawn_room(room_name: String, capacity: u8, size_x: u8, size_y: u8) -> RoomSender {
    let (tx, rx) = mpsc::unbounded();

    debug!("Room `{}`: Created", room_name);

    let room = ServerRoom {
        capacity,
        current_player: "".to_owned(),
        board: Board::new(size_x as usize, size_y as usize),
        players: Vec::new(),
    };

    let (handle_room_abortable, abort_handler) = future::abortable(handle_room(rx, room_name.clone(), room));

    let mut room_tx = tx.clone();
    Task::spawn(async move {
        match future::join(async {
            Timer::after(std::time::Duration::from_secs(15)).await;
            let (tx, mut rx) = mpsc::unbounded();
            match room_tx.send(RoomMessage::RoomCommand(RoomCommand::PlayerNumQuery(tx))).await {
                Err(e) => debug!("Room `{}`: Unable to check for player quantity after room timeout check: {}", room_name.clone(), e),
                Ok(_) => {
                    if let Some(RoomCommand::PlayerNumQueryResponse(val)) = rx.next().await {
                        if val == 0 {
                            trace!("Room: `{}`: Aborting due to no players", room_name);
                            abort_handler.abort();
                        }
                    } else {
                        debug!("Room `{}`: Invalid response for PlayerNumQuery received", room_name.clone());
                    }
                } 
            }
        },
        handle_room_abortable).await
        {
            ((), Err(e)) => debug!("Room `{}` has shutdown with an error: {}", room_name, e),
            ((), Ok(_)) => debug!("Room `{}` has shutdown successfully", room_name)
        }
    }).detach();

    tx
}

fn unable_send_data(room_name: &str, user_name: &str, error: impl std::fmt::Display) {
    warn!("Room `{}`: Username `{}`: Unable to send data to user queue: {}", room_name, user_name, error);
}

async fn handle_room(mut rx: RoomReceiver, room_name: String, mut room: ServerRoom) {
    while let Some(room_message) = rx.next().await {
        match room_message {
            RoomMessage::RoomCommand(room_command) =>
                match room_command {
                    RoomCommand::FullQuery(mut client_tx) => {
                        let mut full = false;

                        if room.players.len() == room.capacity.into() {
                            full = true;
                        }

                        if let Err(e) = client_tx.send(RoomCommand::FullQueryResponse(full)).await {
                            warn!("Room `{}`: Unable to respond to FullQuery: {}", room_name, e);
                        }
                    }

                    RoomCommand::PlayerNumQuery(mut client_tx) => {
                        if let Err(e) = client_tx.send(RoomCommand::PlayerNumQueryResponse(room.players.len() as u8)).await {
                            warn!("Room `{}`: Unable to respond to PlayerNumQuery: {}", room_name, e);
                        }
                    }

                    RoomCommand::PlayerQuery(mut client_tx, username) => {
                        let mut player_exists = false;

                        for player in room.players.iter() {
                            if player.0 == username {
                                player_exists = true;
                            }
                        }

                        if let Err(e) = client_tx.send(RoomCommand::PlayerQueryResponse(player_exists)).await {
                            warn!("Room `{}`: Unable to respond to PlayerQueryResponse: {}", room_name, e);
                        }
                    }

                    RoomCommand::PlayerAdd(username, mut user_tx) => {
                        let mut stone = Stone::Black;
                        
                        if room.players.len() > 0 {
                            if let Some(player) = room.players.get(room.players.len() - 1) {
                                stone = other_cell((player.1).1.stone);
                            }
                        } else {
                            room.current_player = username.clone();
                        }

                        let mut players = Vec::new();
                        room.players.iter()
                            .for_each(|(key, val)| { players.push((key.clone(), val.1.clone())); });

                        let new_player = Player {
                            username: username.clone(),
                            score: 0,
                            stone
                        };

                        players.push((username.clone(), new_player.clone()));

                        let send_room = Room {
                            players,
                            current_player: room.current_player.clone(),
                            self_player: username.clone(),
                            board: room.board.clone(),
                        };

                        let login_message = "`".to_owned() + &username + "` has joined the room";

                        for (_, player) in room.players.iter_mut() {
                            if let Err(e) = player.0.send(ServerMessage::PlayerAdd(new_player.clone())).await {
                                warn!("Room `{}`: Username `{}`: Unable to send room message to user: {}", room_name, username, e);
                            }

                            if let Err(e) = player.0.send(ServerMessage::Chat(login_message.clone())).await {
                                warn!("Room `{}`: Username `{}`: Unable to send room message to user: {}", room_name, username, e);
                            }
                        }

                        room.players.push((username.clone(), (user_tx.clone(), new_player, false)));

                        if let Err(e) = user_tx.send(ServerMessage::LoginResponse(Ok(send_room))).await {
                            warn!("Room `{}`: Username `{}`: Unable to send room message to user: {}", room_name, username, e);
                        }
                    }

                    RoomCommand::PlayerRemove(username) => {
                        let mut player: Option<&mut (String, (mpsc::UnboundedSender::<ServerMessage>, Player, bool))> = None;
                        let mut index = 0;

                        for (i, other_player) in room.players.iter_mut().enumerate() {
                            if other_player.0 == username {
                                player = Some(other_player);
                                index = i;
                            }
                        }

                        if let Some(userdata) = player {
                            if let Err(e) = (userdata.1).0.close().await {
                                error!("Room `{}`: Username `{}`: Unable to remove user: {}", room_name, username, e);
                            }
                            
                            if username == room.current_player {
                                let mut next_player = match room.players.get(0) {
                                    None => {
                                        warn!("Room `{}`: Unable to get first player in room", room_name);
                                        continue;
                                    }

                                    Some((player_name, _)) => player_name.clone()
                                };

                                let mut iter = room.players.iter();

                                for (player_name, _) in &mut iter {
                                    if username == *player_name {
                                        break;
                                    }
                                }

                                if let Some((player_name, _)) = iter.next() {
                                    next_player = player_name.to_string();
                                }

                                room.current_player = next_player;
                            }

                            room.players.remove(index);

                            let quit_message = "`".to_owned() + &username + "` has left the room";

                            for (_, player) in room.players.iter_mut() {
                                if let Err(e) = player.0.send(ServerMessage::PlayerRemove(username.clone())).await {
                                    warn!("Room `{}`: Username `{}`: Unable to send room message to user: {}", room_name, username, e);
                                }

                                if let Err(e) = player.0.send(ServerMessage::NextTurn(room.current_player.clone())).await {
                                    unable_send_data(&room_name[..], &username[..], &e);
                                }

                                if let Err(e) = player.0.send(ServerMessage::Chat(quit_message.clone())).await {
                                    warn!("Room `{}`: Username `{}`: Unable to send room message to user: {}", room_name, username, e);
                                }
                            }
                            debug!("Room `{}`: Username `{}`: Removed from room", room_name, username);
                        } else {
                            error!("Room `{}`: Username `{}`: Unable to find userdata in room", room_name, username);
                        }
                    }

                    _ => warn!("Room `{}`: Invalid RoomCommand received", room_name)
                }

            RoomMessage::ClientMessage(username, client_message) =>
                match client_message {
                    ClientMessage::Place(action) => {
                        if username == room.current_player {
                            match action {
                                // Place Stone
                                Some(position) => {
                                    let mut player: Option<(String, (mpsc::UnboundedSender::<ServerMessage>, Player, bool))> = None;

                                    for other_player in room.players.iter() {
                                        if other_player.0 == username {
                                            player = Some(other_player.clone());
                                        }
                                    }

                                    let player = match player {
                                        None => {
                                            warn!("Room `{}`: Username `{}`: Unable to find userdata in room", room_name, username);
                                            continue;
                                        }

                                        Some(player_value) => player_value.1
                                    };

                                    let stone = player.1.stone;

                                    match room.board.play(true,
                                                          Position(position.x() as usize, position.y() as usize),
                                                          stone)
                                    {
                                        Err(e) => {
                                            if let Err(e2) = player.0.clone().send(ServerMessage::PlaceResponse(Err(e))).await {
                                                unable_send_data(&room_name[..], &username[..], e2);
                                            }
                                        }

                                        Ok(board_delta) => {
                                            let board_delta = match board_delta {
                                                None => {
                                                    warn!("Room `{}`: Username `{}`: On board update following successful move:\
                                                        Board `play` function returned no data", room_name, username);
                                                    continue;
                                                }

                                                Some(data) => data
                                            };

                                            let mut next_player = match room.players.iter().nth(0) {
                                                None => {
                                                    warn!("Room `{}`: Unable to get first player in room", room_name);
                                                    continue;
                                                }

                                                Some((player_name, _)) => player_name.clone()
                                            };

                                            let mut iter = room.players.iter();

                                            for (player_name, _) in &mut iter {
                                                if username == *player_name {
                                                    break;
                                                }
                                            }

                                            if let Some((player_name, _)) = iter.next() {
                                                next_player = player_name.to_string();
                                            }

                                            room.current_player = next_player.clone();

                                            for (_, player) in room.players.iter_mut() {
                                                for stone_move in board_delta.iter() {
                                                    if let Err(e) = player.0.send(stone_move.clone()).await {
                                                        unable_send_data(&room_name[..], &username[..], &e);
                                                    }
                                                }

                                                if let Err(e) = player.0.send(ServerMessage::NextTurn(next_player.clone())).await {
                                                    unable_send_data(&room_name[..], &username[..], &e);
                                                }
                                            }
                                        }
                                    }
                                }
                            
                                // Pass
                                None => {
                                    let pass = ServerMessage::PlaceResponse(Ok(Move(None, Some(username.clone()))));

                                    let mut next_player = match room.players.get(0) {
                                        None => {
                                            warn!("Room `{}`: Unable to get first player in room", room_name);
                                            continue;
                                        }

                                        Some(player_name) => player_name.0.clone()
                                    };

                                    let mut iter = room.players.iter();

                                    for other_player_name in &mut iter {
                                        if other_player_name.0 == username {
                                            break;
                                        }
                                    }

                                    if let Some(other_player) = iter.next() {
                                        next_player = other_player.0.to_string();
                                    }

                                    room.current_player = next_player.clone();

                                    let mut player: Option<&mut (String, (mpsc::UnboundedSender::<ServerMessage>, Player, bool))> = None;

                                    for other_player in room.players.iter_mut() {
                                        if other_player.0 == username {
                                            player = Some(other_player);
                                        }
                                    }

                                    let player: &mut (String, (mpsc::UnboundedSender::<ServerMessage>, Player, bool)) = match player {
                                        None => {
                                            warn!("Room `{}`: Username: `{}`: Unable to find userdata in room", room_name, username);
                                            continue;
                                        }

                                        Some(player_value) => player_value
                                    };

                                    (player.1).2 = true;

                                    for (_, player) in room.players.iter_mut() {
                                        if let Err(e) = player.0.send(pass.clone()).await {
                                            unable_send_data(&room_name[..], &username[..], &e);
                                        }
                                    }

                                    let mut done = true;

                                    for player in room.players.iter() {
                                        if ! (player.1).2 {
                                            done = false;
                                        }
                                    }
                                    
                                    if done {
                                        for player in room.players.iter_mut() {
                                            if let Err(e) = (player.1).0.close().await {
                                                error!("Room `{}`: Username `{}`: Unable to remove user: {}", room_name, username, e);
                                            }
                                        }

                                        //break;
                                    }
                                }
                            }
                        } else {
                            let mut player: Option<(String, (mpsc::UnboundedSender::<ServerMessage>, Player, bool))> = None;

                            for other_player in room.players.iter() {
                                if other_player.0 == username {
                                    player = Some(other_player.clone());
                                }
                            }

                            let mut player = match player {
                                None => {
                                    warn!("Room `{}`: Username: `{}`: Unable to find userdata in room", room_name, username);
                                    continue;
                                }

                                Some(player_value) => player_value.1
                            };

                            if let Err(e) = player.0.send(ServerMessage::PlaceResponse(Err(InvalidMove::InvalidTurn))).await {
                                warn!("Room `{}`: Username `{}`: Unable to send room message to user: {}", room_name, username, e);
                            }
                        }
                    }
                    
                    ClientMessage::Chat(message) => {
                        for (_, player) in room.players.iter_mut() {
                            if let Err(e) = player.0.send(ServerMessage::Chat(message.clone())).await {
                                warn!("Room `{}`: Username `{}`: Unable to send room message to user: {}", room_name, username, e);
                            }
                        }
                    }

                    _ => {
                        let mut player: Option<(mpsc::UnboundedSender::<ServerMessage>, Player, bool)> = None;

                        for other_player in room.players.iter() {
                            if other_player.0 == username {
                                player = Some(other_player.clone().1);
                            }
                        }

                        if let Some(mut userdata) = player {
                            if let Err(e) = userdata.0.send(ServerMessage::AlreadyLoggedIn).await {
                                unable_send_data(&room_name[..], &username[..], &e);
                            }
                        } else {
                            warn!("Room `{}`: Username `{}`: Unable to find userdata in room", room_name, username);
                        }
                    }
                }
        }
    } 

    trace!("Room `{}`: No more messages", room_name);
}

async fn handle_player(house: House,
                       username: String,
                       room_name: String,
                       mut room_tx: RoomSender,
                       ws_combo_stream: WebSocketStream<Async<net::TcpStream>>)
    -> Result<()>
{
    let (ws_sink, ws_stream) = ws_combo_stream.split();
    let (tx, rx) = mpsc::unbounded();
    
    let middleman_tx = ws_stream.filter_map(|raw_wrapped| async move {
            match raw_wrapped {
                Ok(WebSocketMessage::Binary(bin)) => Some(bincode::deserialize::<ClientMessage>(&bin)),
                _ => None,
            }
        })
        .filter_map(|message_wrapped| async move {
            match message_wrapped {
                Ok(message) => Some(message),
                _ => None,
            }
        })
        .map(|message| RoomMessage::ClientMessage(username.clone(), message))
        .chain(futures::stream::once(async {
                RoomMessage::RoomCommand(RoomCommand::PlayerRemove(username.clone()))
        }))
        .map(|message| Ok(message))
        .forward(room_tx.clone());

    let middleman_rx = rx.map(|message| bincode::serialize::<ServerMessage>(&message))
        .filter_map(|bin_wrapped| async move {
            match bin_wrapped {
                Ok(bin) => Some(bin),
                _ => None,
            }
        })
        .map(|bin| Ok(WebSocketMessage::Binary(bin)))
        .forward(ws_sink);


    room_tx.send(RoomMessage::RoomCommand(RoomCommand::PlayerAdd(username.clone(), tx))).await?;

    let (middleman_rx_res, middleman_tx_res) = future::join(middleman_rx, middleman_tx).await;

    if let Err(e) = middleman_rx_res {
        debug!("Room `{}`: Username `{}`: Got error in user queue RX: {}", room_name.clone(), username.clone(), e);
    }

    if let Err(e) = middleman_tx_res {
        debug!("Room `{}`: Username `{}`: Got error in user queue TX: {}", room_name.clone(), username.clone(), e);
    }

    let (query_tx, mut query_rx) = mpsc::unbounded();

    room_tx.send(RoomMessage::RoomCommand(RoomCommand::PlayerNumQuery(query_tx))).await?;

    if let Some(RoomCommand::PlayerNumQueryResponse(0)) = query_rx.next().await {
        let mut lock = house.lock().await;

        if let Some(mut room) = lock.get(&room_name) {
            if let Err(e) = room.close().await {
                error!("Room `{}`: Unable to close", e);
            }
        }

        lock.remove(&room_name);
    }

    Ok(())
}

async fn connection_handle_send_msg<T>(ws_stream: &mut async_tungstenite::WebSocketStream<Async<net::TcpStream>>,
                                       message: T)
    -> Result<()>
where
    T: serde::ser::Serialize 
{
    let error = bincode::serialize::<T>(&message)?;

    ws_stream.send(WebSocketMessage::Binary(error)).await?;

    Ok(())
}

pub async fn connection_handle(raw_stream: Async<net::TcpStream>,
                               socket_addr: net::SocketAddr,
                               house: House)
    -> Result<()>
{
    debug!("[{}]: Accepted TCP Connection", socket_addr);

    let mut ws_stream = async_tungstenite::accept_async(raw_stream).await?;

    while let Some(raw_message_result) = ws_stream.next().await {
        match raw_message_result {
            Err(e) => debug!("[{}]: Error receiving WebSocket message: {}", socket_addr, e),

            Ok(WebSocketMessage::Binary(raw_message)) => {
                if let Ok(message) = bincode::deserialize::<ClientMessage>(&raw_message) {
                    match message {
                        ClientMessage::Login(username, room_name) => {
                            if username.chars().count() > 16 {
                                trace!("[{}]: Username too long", socket_addr);
                                connection_handle_send_msg(&mut ws_stream,
                                                           ServerMessage::LoginResponse(
                                                               Err(LoginError::UsernameTooLong))).await?;

                            } else if room_name.chars().count() > 16 {
                                trace!("[{}]: Room name too long", socket_addr);
                                connection_handle_send_msg(&mut ws_stream,
                                                           ServerMessage::LoginResponse(
                                                               Err(LoginError::RoomNameTooLong))).await?;
                            } else {
                                let mut room_exists = false;

                                if let Some(_) = house.lock().await.get(&room_name) {
                                    room_exists = true;
                                }

                                if room_exists {
                                    let mut room_tx = house.lock().await.get_mut(&room_name).unwrap().clone();
                                    let (tx, mut rx) = mpsc::unbounded();

                                    room_tx.send(RoomMessage::RoomCommand(RoomCommand::PlayerQuery(tx.clone(), username.clone()))).await?;
                                    if let Some(RoomCommand::PlayerQueryResponse(true)) = rx.next().await {
                                        connection_handle_send_msg(&mut ws_stream,
                                                                   ServerMessage::LoginResponse(
                                                                       Err(LoginError::UsernameTaken))).await?;
                                    } else {
                                        room_tx.send(RoomMessage::RoomCommand(RoomCommand::FullQuery(tx.clone()))).await?;
                                        if let Some(RoomCommand::FullQueryResponse(true)) = rx.next().await {
                                            connection_handle_send_msg(&mut ws_stream,
                                                                       ServerMessage::LoginResponse(
                                                                           Err(LoginError::RoomFull))).await?;
                                        } else {
                                            trace!("[{}]: Logging in player `{}` to room `{}`", socket_addr, username, room_name);
                                            handle_player(Arc::clone(&house), username, room_name, room_tx.clone(), ws_stream).await?;
                                            return Ok(())
                                        }
                                    }
                                } else {
                                    trace!("[{}]: Room does not exist", socket_addr);
                                    connection_handle_send_msg(&mut ws_stream,
                                                               ServerMessage::LoginResponse(
                                                                   Err(LoginError::RoomDoesNotExist(room_name)))).await?;
                                }
                            }
                        }

                        ClientMessage::RoomCreate(room_name, capacity, size_x, size_y) => {
                            if room_name.chars().count() > 16 {
                                trace!("[{}]: Room name too long", socket_addr);
                                connection_handle_send_msg(&mut ws_stream,
                                                           ServerMessage::RoomCreateResponse(
                                                               Err(RoomCreateError::RoomNameTooLong))).await?;
                            } else {
                                let mut house_exists = false;

                                if let Some(_) = house.lock().await.get(&room_name) {
                                    house_exists = true;
                                }

                                if house_exists {
                                    trace!("[{}]: Room name already taken", socket_addr);
                                    connection_handle_send_msg(&mut ws_stream,
                                                               ServerMessage::RoomCreateResponse(
                                                                   Err(RoomCreateError::RoomNameTaken))).await?;
                                } else {
                                    trace!("[{}]: Attempting to create room", socket_addr);
                                    house.lock().await.insert(room_name.clone(),
                                                              spawn_room(room_name,
                                                                         capacity,
                                                                         size_x,
                                                                         size_y).await);

                                    connection_handle_send_msg(&mut ws_stream,
                                                               ServerMessage::RoomCreateResponse(
                                                                   Ok(None))).await?;
                                }
                            }
                        }
                        _ => break
                    }
                }
            }

            _ => debug!("[{}]: Wrong type of WebSocket message received", socket_addr)
        }
    }

    debug!("[{}]: Closed TCP Connection", socket_addr);

    Ok(())
}
