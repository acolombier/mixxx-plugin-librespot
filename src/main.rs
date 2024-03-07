#![feature(cursor_remaining)]
use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

use librespot_audio::AudioFetchParams;
use librespot_metadata::audio::AudioFileFormat;
use librespot_metadata::{Metadata, Rootlist};
use librespot_playback::config::{Bitrate, PlayerConfig};
use librespot_playback::mixer::NoOpVolume;
use librespot_playback::player::Player;

use librespot_core::cache::Cache;
use librespot_core::{
    config::SessionConfig,
    session::Session,
    spotify_id::{SpotifyId, SpotifyItemType},
};
use librespot_playback::{
    audio_backend::{Sink, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};
use log::{debug, error, info, warn};
use pb::{
    CloseRequest, CloseResponse, FetchContentRequest, OpenRequest, OpenResponse, SeekRequest,
    SeekResponse, Track, TrackRequest, TrackResponse,
};
use std::cmp;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::vec;
use tokio::net::UnixListener;
use tokio::sync::mpsc::{self};
use tokio::sync::Mutex;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tokio_stream::Stream;
use tonic::transport::server::UdsConnectInfo;
use tonic::Code;
use tonic::{transport::Server, Request, Response, Status};

pub mod pb {
    tonic::include_proto!("mixxx.plugin");
}

use pb::{
    plugin_service_server::{PluginService, PluginServiceServer},
    track_service_server::{TrackService, TrackServiceServer},
    tracklist_service_server::{TracklistService, TracklistServiceServer},
    view_event::ViewEventOneof,
    BrowseReply, BrowseRequest, ManifestReply, ManifestRequest, Node, NodeType, ReadChunk,
    ReadRequest, SideEffect, ViewEvent,
};

mod audio;
mod view;

use audio::loader::TrackLoader;
use view::login::{get_qml_view, LoginForm};

use crate::pb::{SearchMode, Tracklist};

#[derive(Clone, Default)]
pub struct Plugin {
    state: Arc<Mutex<PluginState>>,
}

enum SessionStatus {
    Disconnect,
    Failed(String),
    Connected(Box<Rootlist>),
}

struct PluginState {
    session: Session,
    status: SessionStatus,
    loader: Arc<tokio::sync::Mutex<TrackLoader>>,
    player: Arc<Player>,
}

impl Default for PluginState {
    fn default() -> Self {
        let config = SessionConfig::default();
        // config.proxy = Some(Url::parse("http://127.0.0.1:8080").unwrap());
        let session = Session::new(
            config,
            Cache::new(
                "./spotcache".into(),
                None,
                "./spotcache".into(),
                Some(1_000_000_000),
            )
            .ok(),
        );

        PluginState {
            loader: Arc::new(tokio::sync::Mutex::new(TrackLoader::new(session.clone()))),
            status: SessionStatus::Disconnect,
            player: Player::new(
                PlayerConfig {
                    bitrate: Bitrate::Bitrate320,
                    ..PlayerConfig::default()
                },
                session.clone(),
                Box::new(NoOpVolume),
                move || Box::new(EmptySink {}),
            ),
            session,
        }
    }
}

#[derive(PartialEq)]
pub enum PlaylistType {
    ForYou,
    Followed,
    MyOwn,
}

impl TryFrom<std::string::String> for PlaylistType {
    type Error = &'static str;

    fn try_from(value: std::string::String) -> Result<Self, Self::Error> {
        let value = value.trim_matches(|c| !char::is_alphabetic(c));
        match value.trim_matches(|c| !char::is_alphabetic(c)) {
            "followedplaylist" => Ok(PlaylistType::Followed),
            "myplaylist" => Ok(PlaylistType::MyOwn),
            "foryou" => Ok(PlaylistType::ForYou),
            _ => Err("Unknown playlist type"),
        }
    }
}

impl Plugin {
    async fn get_root_node(&self) -> Result<BrowseReply, Status> {
        let state = self.state.lock().await;
        match &state.status {
            SessionStatus::Connected(profile) => {
                info!("{:?}", profile);
                Ok(BrowseReply {
                    nodes: vec![
                        Node {
                            r#type: NodeType::Node.into(),
                            label: "Followed playlists".to_owned(),
                            id: "/followedplaylist/".to_owned(),
                            icon: vec![],
                        },
                        Node {
                            r#type: NodeType::Node.into(),
                            label: "My Playlists".to_owned(),
                            id: "/myplaylist/".to_owned(),
                            icon: vec![],
                        },
                        Node {
                            r#type: NodeType::Node.into(),
                            label: "For you".to_owned(),
                            id: "/foryou/".to_owned(),
                            icon: vec![],
                        },
                    ],
                    ..BrowseReply::default()
                })
            }
            SessionStatus::Disconnect | SessionStatus::Failed(_) => Ok(BrowseReply {
                view: get_qml_view().map_err(|e| {
                    error!("Unable to open root view: {}", e);
                    Status::new(Code::Unimplemented, "Unable to fetch root view")
                })?,
                ..BrowseReply::default()
            }),
        }
    }
    async fn get_playlist_node(
        &self,
        state: &PluginState,
        rootlist: &Rootlist,
        playlist_type: PlaylistType,
    ) -> Result<BrowseReply, Status> {
        let mut playlists: Vec<(String, SpotifyId)> = rootlist
            .contents
            .meta_items
            .iter()
            .zip(rootlist.contents.items.iter())
            .filter_map(|(meta, item)| match meta.owner_username.as_str() {
                "spotify" if playlist_type == PlaylistType::ForYou => {
                    Some((meta.attributes.name.to_owned(), item.id))
                }
                me if me == state.session.username() && playlist_type == PlaylistType::MyOwn => {
                    Some((meta.attributes.name.to_owned(), item.id))
                }
                _ if playlist_type == PlaylistType::Followed => {
                    Some((meta.attributes.name.to_owned(), item.id))
                }
                _ => None,
            })
            .collect();
        playlists.sort_by_key(|i| i.0.to_owned());
        Ok(BrowseReply {
            nodes: playlists
                .iter()
                .map(|p| Node {
                    r#type: NodeType::Leaf.into(),
                    label: p.0.to_owned(),
                    id: p.1.to_string(),
                    icon: vec![],
                })
                .collect(),
            tracklist: Option::None,
            view: "".into(),
        })
    }
    async fn get_node(&self, node: &Node) -> Result<BrowseReply, Status> {
        let state = self.state.lock().await;
        match &state.status {
            SessionStatus::Connected(rootlist) => {
                if node.id.starts_with("spotify:playlist") {
                    let plist_uri = SpotifyId::from_uri(&node.id).map_err(|e| {
                        Status::new(
                            Code::InvalidArgument,
                            format!("Couldn't parse the playlist id: {:}", e),
                        )
                    })?;

                    let plist = librespot_metadata::Playlist::get(&state.session, &plist_uri)
                        .await
                        .unwrap();
                    info!("{:?}", plist);

                    Ok(BrowseReply {
                        nodes: vec![],
                        tracklist: Some(Tracklist {
                            r#ref: node.id.to_owned(),
                            id: plist_uri.id as i64,
                            search: SearchMode::None.into(),
                            track_count: plist.length,
                        }),
                        view: "".into(),
                    })
                } else {
                    self.get_playlist_node(
                        &state,
                        rootlist,
                        node.id.clone().try_into().map_err(|_| {
                            Status::new(Code::Unimplemented, "Unrecognised node type")
                        })?,
                    )
                    .await
                }
            }
            SessionStatus::Disconnect => {
                Err(Status::new(Code::Unauthenticated, "No session is active"))
            }
            SessionStatus::Failed(e) => Err(Status::new(
                Code::Unauthenticated,
                format!("Unable to start a session: {:}", e).to_owned(),
            )),
        }
    }
}

struct EmptySink;
impl Sink for EmptySink {
    fn write(&mut self, _: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        Ok(())
    }
}

#[tonic::async_trait]
impl PluginService for Plugin {
    async fn manifest(
        &self,
        request: Request<ManifestRequest>,
    ) -> Result<Response<ManifestReply>, Status> {
        let conn_info = request.extensions().get::<UdsConnectInfo>().unwrap();
        info!("Got a request {:?} with info {:?}", request, conn_info);

        let reply = ManifestReply {
            name: "Spotify".into(),
            version: "0.1.0".into(),
            icon: vec![],
        };
        Ok(Response::new(reply))
    }
    async fn browse(
        &self,
        request: Request<BrowseRequest>,
    ) -> Result<Response<BrowseReply>, Status> {
        let conn_info = request.extensions().get::<UdsConnectInfo>().unwrap();
        info!("Got a request {:?} with info {:?}", request, conn_info);

        let reply = match request.into_inner().node {
            None => self.get_root_node().await?,
            Some(node) => self.get_node(&node).await?,
        };

        Ok(Response::new(reply))
    }
    async fn event(&self, request: Request<ViewEvent>) -> Result<Response<SideEffect>, Status> {
        let conn_info = request.extensions().get::<UdsConnectInfo>().unwrap();
        info!("Got a request {:?} with info {:?}", request, conn_info);

        match request.into_inner().view_event_oneof {
            Some(ViewEventOneof::Submit(event)) => {
                let form = serde_urlencoded::from_bytes::<LoginForm>(&event.payload).unwrap();
                info!("login with {:?}", form);

                info!("Connecting...");

                let lock = Arc::clone(&self.state);
                let mut state = lock.lock().await;
                state.status = match state.session.connect(form.into(), true).await {
                    Ok(()) => {
                        info!("Connected!");
                        SessionStatus::Connected(Box::new(
                            librespot_metadata::Rootlist::get(
                                &state.session,
                                &SpotifyId {
                                    id: 0,
                                    item_type: SpotifyItemType::Unknown,
                                },
                            )
                            .await
                            .map_err(|e| Status::new(Code::InvalidArgument, e.to_string()))?,
                        ))
                    }
                    Err(e) => {
                        info!("Error connecting: {}", e);
                        SessionStatus::Failed(e.to_string())
                    }
                };
                Ok(Response::new(SideEffect::default()))
            }
            Some(evt) => {
                warn!("unsupported event received: {:?}", evt);
                Ok(Response::new(SideEffect::default()))
            }
            None => {
                debug!("empty event received");
                Ok(Response::new(SideEffect::default()))
            }
        }
    }
}

impl From<librespot_metadata::Track> for Track {
    fn from(value: librespot_metadata::Track) -> Self {
        Self {
            id: value.id.id as i64,
            r#ref: value.id.to_string(),
            title: value.original_title,
            artist: value
                .artists
                .iter()
                .map(|a| a.name.to_owned())
                .collect::<Vec<_>>()
                .join(", "),
            album: value.album.name,
            artwork: vec![],
        }
    }
}

#[tonic::async_trait]
impl TrackService for Plugin {
    async fn get(&self, req: Request<TrackRequest>) -> Result<Response<TrackResponse>, Status> {
        let req = req.into_inner();
        let track_ref = if req.r#ref.starts_with('/') {
            req.r#ref[1..].to_owned()
        } else {
            req.r#ref
        };
        let track = SpotifyId::from_uri(&track_ref).map_err(|_| {
            Status::new(
                Code::InvalidArgument,
                format!("ref {:} is invalid", track_ref),
            )
        })?;
        if track.item_type != SpotifyItemType::Track {
            return Err(Status::new(
                Code::InvalidArgument,
                format!("ref {:} is not a track", track_ref),
            ));
        }

        let lock = Arc::clone(&self.state);
        let state = lock.lock().await;

        librespot_metadata::Track::get(&state.session, &track)
            .await
            .map(|t| {
                Response::new(TrackResponse {
                    track: Some(t.into()),
                })
            })
            .map_err(|e| Status::new(Code::Unavailable, format!("unable to get track: {:}", e)))
    }
    async fn open(&self, req: Request<OpenRequest>) -> Result<Response<OpenResponse>, Status> {
        let req = req.into_inner();
        let track_ref = req.track.unwrap().r#ref;
        let track = SpotifyId::from_uri(&track_ref).map_err(|_| {
            Status::new(
                Code::InvalidArgument,
                format!("ref {:} is not a track", track_ref),
            )
        })?;

        let lock = Arc::clone(&self.state);
        let state = lock.lock().await;

        let loader_lock = Arc::clone(&state.loader);
        let mut loader = loader_lock.lock().await;

        state.player.preload(track);
        let (filesize, format) = loader
            .open(track)
            .await
            .map_err(|e| Status::new(Code::Unavailable, e))?;
        let mime = match format {
            AudioFileFormat::OGG_VORBIS_320
            | AudioFileFormat::OGG_VORBIS_160
            | AudioFileFormat::OGG_VORBIS_96 => "application/ogg",
            AudioFileFormat::MP3_320
            | AudioFileFormat::MP3_256
            | AudioFileFormat::MP3_160
            | AudioFileFormat::MP3_96 => "audio/mpeg",
            _ => "application/octet-stream",
        }
        .to_owned();
        Ok(Response::new(OpenResponse { filesize, mime }))
    }
    type ReadStream = Pin<Box<dyn Stream<Item = Result<ReadChunk, Status>> + Send + Sync>>;
    async fn read(&self, req: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let req = req.into_inner();

        let mut track = SpotifyId::from_uri(&req.track.unwrap().r#ref)
            .map_err(|_| Status::new(Code::InvalidArgument, "track id is invalid"))?;
        track.item_type = SpotifyItemType::Track;
        info!("Playing...");

        let chunk_size: usize = if req.chunk_size == 0 {
            10_240
        } else {
            cmp::max(128, cmp::min(req.chunk_size as usize, 10_240))
        };
        let offset = req.offset;
        let limit = req.limit as usize;

        let (tx, rx) = mpsc::channel(4);

        let lock = Arc::clone(&self.state);
        let state = lock.lock().await;
        let loader_lock = Arc::clone(&state.loader);

        tokio::spawn(async move {
            let mut loader = loader_lock.lock().await;

            if let Some(loaded_track) = loader.get_opened_mut(&track) {
                let mut read: usize = 0;
                if let Err(e) = loaded_track.seek(SeekFrom::Start(offset)) {
                    tx.send(Result::<_, Status>::Err(Status::new(
                        Code::InvalidArgument,
                        format!("Couldn't seek in file: {:}", e),
                    )))
                    .await
                    .unwrap();
                    return;
                }

                info!("Reading up to {:} from {:}...", limit, offset);
                loop {
                    let mut buffer: Vec<u8> = vec![0; cmp::min(chunk_size, limit - read)];
                    info!("Reading chunk of {:}...", buffer.len());
                    match loaded_track.read(&mut buffer) {
                        Ok(readsize) => {
                            read += readsize;
                            match tx
                                .send(Result::<_, Status>::Ok(ReadChunk {
                                    data: buffer[0..readsize].to_vec(),
                                    eof: readsize == 0,
                                }))
                                .await
                            {
                                Ok(_) => {
                                    // item (server response) was queued to be send to client
                                }
                                Err(_item) => {
                                    // output_stream was build from rx and both are dropped
                                    break;
                                }
                            };
                            if readsize == 0 {
                                info!("Reach EOF after {:}...", read);
                                break;
                            } else if read >= limit {
                                info!("Read {:}...", read);
                                break;
                            }
                        }
                        Err(e) => {
                            if let Err(e2) = tx
                                .send(Result::<_, Status>::Err(Status::new(
                                    Code::Internal,
                                    format!("Cannot read track: {:}", e),
                                )))
                                .await
                            {
                                error!("Unable to send error to client while reading {}: {} (Error was:{})", track, e2, e);
                            }
                            break;
                        }
                    };
                }
                info!("Done reading with underrun of {:}...", limit);
            } else {
                tx.send(Result::<_, Status>::Err(Status::new(
                    Code::InvalidArgument,
                    "No track is currently open",
                )))
                .await
                .unwrap();
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream) as Self::ReadStream))
    }
    async fn seek(&self, req: Request<SeekRequest>) -> Result<Response<SeekResponse>, Status> {
        let req = req.into_inner();
        let track = SpotifyId::from_uri(&req.track.unwrap().r#ref)
            .map_err(|_| Status::new(Code::InvalidArgument, "track id is invalid"))?;
        let position = req.position;

        let lock = Arc::clone(&self.state);
        let state = lock.lock().await;

        let loader_lock = Arc::clone(&state.loader);
        let mut loader = loader_lock.lock().await;

        Ok(Response::new(SeekResponse {
            position: loader.seek(&track, position).map_err(|e| {
                Status::new(Code::Internal, format!("Couldn't seek in file: {:}", e))
            })?,
        }))
    }
    async fn close(&self, req: Request<CloseRequest>) -> Result<Response<CloseResponse>, Status> {
        let req = req.into_inner();
        let track: SpotifyId = SpotifyId::from_uri(&req.track.unwrap().r#ref)
            .map_err(|_| Status::new(Code::InvalidArgument, "track id is invalid"))?;

        let lock = Arc::clone(&self.state);
        let state = lock.lock().await;

        let loader_lock = Arc::clone(&state.loader);
        let mut loader = loader_lock.lock().await;

        loader
            .close(&track)
            .map_err(|e| Status::new(Code::Internal, e))?;
        Ok(Response::new(CloseResponse {}))
    }
}

#[tonic::async_trait]
impl TracklistService for Plugin {
    type FetchContentStream = Pin<Box<dyn Stream<Item = Result<Track, Status>> + Send + Sync>>;
    async fn fetch_content(
        &self,
        req: Request<FetchContentRequest>,
    ) -> Result<Response<Self::FetchContentStream>, Status> {
        let args = req.into_inner();

        let plist_uri = SpotifyId::from_uri(&args.tracklist.unwrap().r#ref).map_err(|e| {
            Status::new(
                Code::InvalidArgument,
                format!("Couldn't parse the playlist id: {:}", e),
            )
        })?;

        let (tx, rx) = mpsc::channel(4);

        let lock = Arc::clone(&self.state);
        tokio::spawn(async move {
            let state = lock.lock().await;

            let plist = librespot_metadata::Playlist::get(&state.session, &plist_uri)
                .await
                .unwrap();
            info!("{:?}", plist);

            let tracks: Vec<_> = plist.tracks().collect();
            let offset = args.offset;
            let mut limit = args.limit;

            limit = if limit > 0 {
                limit + offset
            } else {
                tracks.len() as i32
            };

            for i in offset..limit {
                let track_id = tracks.get(i as usize).unwrap();
                let track = librespot_metadata::Track::get(&state.session, track_id)
                    .await
                    .unwrap();
                info!("track: {} ", track.name);
                match tx.send(Result::<Track, Status>::Ok(track.into())).await {
                    Ok(_) => {
                        // item (server response) was queued to be send to client
                    }
                    Err(_item) => {
                        // output_stream was build from rx and both are dropped
                        return;
                    }
                }
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(output_stream) as Self::FetchContentStream
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let path = "/tmp/mixxx_plugin_test.sock";

    if std::fs::remove_file(Path::new(path)).is_ok() {
        info!("Removing old socket")
    }
    std::fs::create_dir_all(Path::new(path).parent().unwrap())?;

    let plugin: Plugin = Plugin::default();

    AudioFetchParams::set(AudioFetchParams {
        read_ahead_before_playback: Duration::from_secs(5),
        read_ahead_during_playback: Duration::from_secs(30),
        prefetch_threshold_factor: 4.0,
        ..AudioFetchParams::default()
    })
    .map_err(|_| "Unable to set AudioFetchParams")?;

    let lock = Arc::clone(&plugin.state);
    tokio::spawn(async move {
        let mut state = lock.lock().await;
        if let Some(cache) = state.session.cache() {
            if let Some(cred) = cache.credentials() {
                state.status = match state.session.connect(cred, true).await {
                    Ok(()) => {
                        info!("Connected with cached credentials");
                        match librespot_metadata::Rootlist::get(
                            &state.session,
                            &SpotifyId {
                                id: 0,
                                item_type: SpotifyItemType::Unknown,
                            },
                        )
                        .await
                        {
                            Ok(rootlist) => SessionStatus::Connected(Box::new(rootlist)),
                            Err(e) => {
                                error!("Cannot fetch rootlist with cached credentials: {:}", e);
                                SessionStatus::Failed(e.error.to_string())
                            }
                        }
                    }
                    Err(e) => {
                        error!("Cannot connect with cached credentials: {:}", e);
                        SessionStatus::Failed(e.error.to_string())
                    }
                };
            }
        }
    });

    let uds = UnixListener::bind(path)?;
    let uds_stream = UnixListenerStream::new(uds);

    Server::builder()
        .add_service(TrackServiceServer::new(plugin.clone()))
        .add_service(TracklistServiceServer::new(plugin.clone()))
        .add_service(PluginServiceServer::new(plugin))
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}
