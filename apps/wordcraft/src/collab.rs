//! Shared editing for WordCraft, over the CraftCollab relay.
//!
//! The merge and the socket live in [`craft_collab`]. This file is the WordCraft mapping:
//! paragraphs as scopes, `text.insert` as lock-free CRDT, everything else still locked.
//! Desktop uses [`craft_collab::native::NativeHub`]; the browser crate includes this file
//! and uses [`craft_collab::web::WebHub`].

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use craft_collab::{Live, Work, doc_from_token};
use serde_json::Value as Json;
use wordcraft_engine::doc::{Pos, StoryRef};
use wordcraft_engine::{CollabSink, Selection, Session as Engine};

#[cfg(not(target_arch = "wasm32"))]
use craft_collab::native::NativeHub;
#[cfg(target_arch = "wasm32")]
use craft_collab::web::WebHub;

/// The key the sender's selection travels under.
///
/// The relay moves `params` through untouched and compares nothing inside it, so a key of our own
/// is safe there. No WordCraft command reads it either: each one looks up the parameter names it
/// wants and ignores everything else.
const AT: &str = "$collabAt";

/// Where the relay credential comes from.
///
/// A client never holds the signing secret. The relay verifies an HMAC that the service produced,
/// so all a client can do is present a token it was handed. Anything able to mint tokens could
/// forge any file id and any permission bits, and that is why the key stays server-side.
pub trait TokenSource: Send {
    /// Where the relay is, and a signed token for one document.
    fn join(&self) -> Option<(String, String)>;
}

/// Reads the environment.
pub struct EnvToken;

#[cfg(not(target_arch = "wasm32"))]
fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

#[cfg(not(target_arch = "wasm32"))]
impl TokenSource for EnvToken {
    fn join(&self) -> Option<(String, String)> {
        Some((env("COLLABCRAFT_RELAY")?, env("COLLABCRAFT_TOKEN")?))
    }
}

/// One local edit on its way out: the command, its parameters with the sender's selection in them,
/// and the region it belongs to.
struct Edit {
    id: String,
    params: Json,
    scope: String,
}

/// The engine-facing half, held by the engine as an `Arc<dyn CollabSink>`.
struct Outbound(Sender<Edit>);

impl CollabSink for Outbound {
    fn on_local(&self, id: &str, params: &Json, at: &Selection, scope: &str) {
        let mut params = params.clone();
        match (params.as_object_mut(), serde_json::to_value(at)) {
            (Some(object), Ok(at)) => {
                object.insert(AT.to_owned(), at);
            }
            _ => log::debug!("collab: {id} carries no position"),
        }
        let _ = self.0.send(Edit { id: id.to_owned(), params, scope: scope.to_owned() });
    }
}

/// Pull the sender's selection back out of a command's parameters.
fn take_position(mut params: Json) -> (Json, Option<Selection>) {
    let at = params.as_object_mut().and_then(|object| object.remove(AT)).and_then(|at| serde_json::from_value(at).ok());
    (params, at)
}

/// A live shared-editing link.
pub struct Link {
    #[cfg(not(target_arch = "wasm32"))]
    live: Live<NativeHub>,
    #[cfg(not(target_arch = "wasm32"))]
    _rt: tokio::runtime::Runtime,
    #[cfg(target_arch = "wasm32")]
    live: Live<WebHub>,
    local: Receiver<Edit>,
    outbound: Sender<Edit>,
    /// Lines for the user, drained by the app each frame.
    pub notices: Vec<String>,
    /// Who else is in the document.
    pub roster: Vec<(String, String)>,
}

impl Link {
    /// Reaches the relay on a worker. The first paint does not wait.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn spawn(src: impl TokenSource + 'static) -> Receiver<Link> {
        let (tx, rx) = channel();
        let _ = std::thread::Builder::new().name("collab-connect".into()).spawn(move || {
            if let Some(link) = Link::connect(&src) {
                let _ = tx.send(link);
            }
        });
        rx
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn connect(src: &dyn TokenSource) -> Option<Link> {
        let (relay, token) = src.join()?;
        let Some(doc) = doc_from_token(&token) else {
            log::warn!("collab: the token carries no file id, so there is no room to join");
            return None;
        };

        let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                log::warn!("collab: no async runtime: {e}");
                return None;
            }
        };
        let connect = tokio::time::timeout(std::time::Duration::from_secs(5), NativeHub::connect(&relay, &token));
        let hub = match rt.block_on(connect) {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                log::warn!("collab: cannot reach the relay at {relay}: {e}");
                return None;
            }
            Err(_) => {
                log::warn!("collab: the relay at {relay} did not answer in time");
                return None;
            }
        };

        let (outbound, local) = channel();
        log::info!("collab: joining {doc} on {relay}");
        Some(Link {
            live: Live::new(hub, doc),
            _rt: rt,
            local,
            outbound,
            notices: Vec::new(),
            roster: Vec::new(),
        })
    }

    /// A browser invite already opened the socket. Hold this for as long as the document is open.
    #[cfg(target_arch = "wasm32")]
    pub fn from_web(hub: WebHub, room: &str) -> Link {
        let (outbound, local) = channel();
        log::info!("collab: joining {room} from the browser");
        Link {
            live: Live::new(hub, room),
            local,
            outbound,
            notices: Vec::new(),
            roster: Vec::new(),
        }
    }

    /// Hands the engine the outbound sink.
    pub fn bind(&mut self, engine: &mut Engine) {
        self.seed_from(engine);
        engine.collab = Some(Arc::new(Outbound(self.outbound.clone())));
    }

    fn seed_from(&mut self, engine: &Engine) {
        for path in engine.doc.para_paths(StoryRef::Body) {
            if let Some(para) = engine.doc.para(StoryRef::Body, &path) {
                let at = Pos { story: StoryRef::Body, path, off: 0 };
                self.live.texts().seed(&wordcraft_engine::collab::scope_of(&at), &para.text);
            }
        }
    }

    /// One frame's work: send what the user did, apply what arrived.
    pub fn pump(&mut self, engine: &mut Engine) {
        while let Ok(edit) = self.local.try_recv() {
            if edit.id == "text.insert" {
                if let Err(e) = self.send_insert(&edit, engine) {
                    self.live.notices.push(format!("that edit did not reach the others: {e}"));
                }
                continue;
            }
            if let Err(e) = self.live.submit(edit.id, edit.params, Some(edit.scope)) {
                self.live.notices.push(format!("that edit did not reach the others: {e}"));
            }
        }

        for work in self.live.pump() {
            match work {
                Work::Text { scope, rendered } => {
                    if let Err(e) = engine.apply_crdt_paragraph(&scope, &rendered) {
                        log::warn!("collab: a remote crdt.text did not apply: {e}");
                        self.live.notices.push(format!("could not apply a change from another editor: {e}"));
                    }
                }
                Work::Command { command, params } => {
                    let (params, at) = take_position(params);
                    if let Err(e) = engine.apply_remote(&command, &params, at) {
                        log::warn!("collab: a remote {command} did not apply: {e}");
                        self.live.notices.push(format!("could not apply a change from another editor: {e}"));
                    }
                }
            }
        }
        self.notices = std::mem::take(&mut self.live.notices);
        self.roster.clone_from(&self.live.roster);
    }

    fn send_insert(&mut self, edit: &Edit, engine: &Engine) -> craft_collab::Result<()> {
        let inserted = match edit.params.get("text").and_then(Json::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return self.live.submit(edit.id.clone(), edit.params.clone(), Some(edit.scope.clone())),
        };
        let at: Selection = match edit.params.get(AT).cloned().and_then(|v| serde_json::from_value(v).ok()) {
            Some(at) => at,
            None => return self.live.submit(edit.id.clone(), edit.params.clone(), Some(edit.scope.clone())),
        };
        let Some(pos) = wordcraft_engine::collab::parse_scope(&edit.scope) else {
            return self.live.submit(edit.id.clone(), edit.params.clone(), Some(edit.scope.clone()));
        };
        let Some(para) = engine.doc.para_at(&pos) else {
            return self.live.submit(edit.id.clone(), edit.params.clone(), Some(edit.scope.clone()));
        };
        if self.live.texts().render(&edit.scope).is_none() {
            let prefix = para.text.get(..at.focus.off.min(para.text.len())).unwrap_or("");
            let start = at.focus.off.saturating_add(inserted.len());
            let suffix = para.text.get(start..).unwrap_or("");
            let mut before = String::with_capacity(prefix.len() + suffix.len());
            before.push_str(prefix);
            before.push_str(suffix);
            self.live.texts().seed(&edit.scope, &before);
        }
        let prefix = para.text.get(..at.focus.off.min(para.text.len())).unwrap_or("");
        self.live.insert_text(&edit.scope, prefix.chars().count(), inserted)
    }
}
