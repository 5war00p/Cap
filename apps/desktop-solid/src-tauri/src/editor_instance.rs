use crate::playback::{self, PlaybackHandle};
use crate::{editor, AudioData};
use cap_project::ProjectConfiguration;
use cap_rendering::{ProjectUniforms, RenderOptions, RenderVideoConstants, VideoDecoderActor};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{path::PathBuf, process::Command, sync::Arc};
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, Mutex};

pub struct EditorState {
    pub playhead_position: u32,
    pub playback_task: Option<PlaybackHandle>,
}

pub struct EditorInstance {
    pub path: PathBuf,
    pub id: String,
    pub screen_decoder: VideoDecoderActor,
    pub camera_decoder: Option<VideoDecoderActor>,
    pub audio: Option<AudioData>,
    pub ws_port: u16,
    pub renderer: Arc<editor::RendererHandle>,
    pub render_constants: Arc<RenderVideoConstants>,
    pub state: Mutex<EditorState>,
    on_state_change: Box<dyn Fn(&EditorState) + Send + Sync + 'static>,
    rendering: Arc<AtomicBool>,
}

impl EditorInstance {
    pub async fn new(
        projects_path: PathBuf,
        video_id: String,
        on_state_change: impl Fn(&EditorState) + Send + Sync + 'static,
    ) -> Self {
        let project_path = projects_path
            // app
            //     .path()
            //     .app_data_dir()
            //     .unwrap()
            //     .join("recordings")
            .join(format!("{video_id}.cap"));

        if !project_path.exists() {
            println!("Video path {} not found!", project_path.display());
            // return Err(format!("Video path {} not found!", path.display()));
            panic!("Video path {} not found!", project_path.display());
        }

        let meta = cap_project::RecordingMeta::load_for_project(&project_path);

        const OUTPUT_SIZE: (u32, u32) = (1920, 1080);

        let render_options = RenderOptions {
            screen_size: (meta.display.width, meta.display.height),
            camera_size: meta.camera.as_ref().map(|c| (c.width, c.height)), //.unwrap_or((0, 0)),
            output_size: OUTPUT_SIZE,
        };

        let screen_decoder = VideoDecoderActor::new(project_path.join(meta.display.path).clone());
        let camera_decoder = meta
            .camera
            .map(|camera| VideoDecoderActor::new(project_path.join(camera.path).clone()));

        let audio = meta.audio.map(|audio| {
            let audio_path = project_path.join(audio.path);

            let stdout = Command::new("ffmpeg")
                .arg("-i")
                .arg(audio_path)
                .args(["-f", "f64le", "-acodec", "pcm_f64le"])
                .args(["-ar", &audio.sample_rate.to_string()])
                .args(["-ac", &audio.channels.to_string(), "-"])
                .output()
                .unwrap()
                .stdout;

            let buffer = stdout
                .chunks_exact(8)
                .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                .collect::<Vec<_>>();

            println!("audio buffer length: {}", buffer.len());

            AudioData {
                buffer: Arc::new(buffer),
                sample_rate: audio.sample_rate,
            }
        });

        let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel();

        let ws_port = create_frames_ws(frame_rx).await;

        let render_constants = Arc::new(RenderVideoConstants::new(render_options).await.unwrap());

        let renderer = Arc::new(editor::Renderer::spawn(render_constants.clone(), frame_tx));

        Self {
            id: video_id,
            path: project_path,
            screen_decoder,
            camera_decoder,
            ws_port,
            renderer,
            render_constants,
            audio,
            state: Mutex::new(EditorState {
                playhead_position: 0,
                playback_task: None,
            }),
            rendering: Arc::new(AtomicBool::new(false)),
            on_state_change: Box::new(on_state_change),
        }
    }

    pub async fn dispose(&self) {
        let mut state = self.state.lock().await;
        println!("got state");
        if let Some(handle) = state.playback_task.take() {
            println!("stopping playback");
            handle.stop();
        };
    }

    pub async fn modify_and_emit_state(&self, modify: impl Fn(&mut EditorState)) {
        let mut state = self.state.lock().await;
        modify(&mut state);
        (self.on_state_change)(&state);
    }

    pub async fn start_playback(self: Arc<Self>, project: ProjectConfiguration) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };

        let start_frame_number = state.playhead_position;

        let playback_handle = playback::Playback {
            audio: self.audio.clone(),
            renderer: self.renderer.clone(),
            render_constants: self.render_constants.clone(),
            screen_decoder: self.screen_decoder.clone(),
            camera_decoder: self.camera_decoder.clone(),
            start_frame_number,
            project,
        }
        .start()
        .await;

        let prev = state.playback_task.replace(playback_handle.clone());

        drop(state);

        let mut handle = playback_handle;
        tokio::spawn(async move {
            loop {
                let event = *handle.receive_event().await;

                match event {
                    playback::PlaybackEvent::Start => {}
                    playback::PlaybackEvent::Frame(frame_number) => {
                        self.modify_and_emit_state(|state| {
                            state.playhead_position = frame_number;
                        })
                        .await;
                    }
                    playback::PlaybackEvent::Stop => {
                        return;
                    }
                }
            }
        });

        if let Some(prev) = prev {
            prev.stop();
        }
    }

    pub fn try_render_frame(self: &Arc<Self>, frame_number: u32, project: ProjectConfiguration) {
        if self.rendering.load(Ordering::Relaxed) {
            return;
        }

        let this = self.clone();

        tokio::spawn(async move {
            this.rendering.store(true, Ordering::Relaxed);

            let Some(screen_frame) = this.screen_decoder.get_frame(frame_number).await else {
                return;
            };

            let camera_frame = match &this.camera_decoder {
                Some(d) => d.get_frame(frame_number).await,
                None => None,
            };

            this.renderer
                .render_frame(
                    screen_frame,
                    camera_frame,
                    project.background.source.clone(),
                    ProjectUniforms::new(&this.render_constants, &project),
                )
                .await;

            this.rendering.store(false, Ordering::Relaxed);
        });
    }
}

async fn create_frames_ws(frame_rx: mpsc::UnboundedReceiver<Vec<u8>>) -> u16 {
    use axum::{
        extract::{
            ws::{Message, WebSocket, WebSocketUpgrade},
            State,
        },
        response::IntoResponse,
        routing::get,
    };
    use tokio::sync::{mpsc::UnboundedReceiver, Mutex};

    type RouterState = Arc<Mutex<UnboundedReceiver<Vec<u8>>>>;

    async fn ws_handler(
        ws: WebSocketUpgrade,
        State(state): State<RouterState>,
    ) -> impl IntoResponse {
        // let rx = rx.lock().await.take().unwrap();
        ws.on_upgrade(move |socket| handle_socket(socket, state))
    }

    async fn handle_socket(mut socket: WebSocket, state: RouterState) {
        let mut rx = state.lock().await;
        println!("socket connection established");
        let now = std::time::Instant::now();

        loop {
            tokio::select! {
                _ = socket.recv() => {
                    break;
                }
                msg = rx.recv() => {
                    if let Some(chunk) = msg {
                        socket.send(Message::Binary(chunk)).await.unwrap();
                    }
                }
            }
        }
        let elapsed = now.elapsed();
        println!("Websocket closing after {elapsed:.2?}");
    }

    let router = axum::Router::new()
        .route("/frames-ws", get(ws_handler))
        .with_state(Arc::new(Mutex::new(frame_rx)));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service())
            .await
            .unwrap();
    });

    port
}
