use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{delete, get},
    Extension, Form, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast::{channel, Sender};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt as _};

pub type TodosStream = Sender<TodoUpdate>;

#[derive(Clone, Serialize, Debug)]
pub enum MutationKind {
    Create,
    Delete,
}

#[derive(Clone, Serialize, Debug)]
pub struct TodoUpdate {
    mutation_kind: MutationKind,
    id: i32,
}
#[derive(Clone, Serialize, Deserialize)]
struct Todo {
    id: i32,
    description: String,
}

#[derive(Serialize, Deserialize)]
struct TodoNew {
    description: String,
}

#[derive(Clone)]
struct AppState {
    todos: Arc<Mutex<HashMap<i32, Todo>>>,
    idx: Arc<Mutex<i32>>,
}

#[tokio::main]
async fn main() {
    let (tx, _rx) = channel::<TodoUpdate>(10);
    let state = AppState {
        todos: Arc::new(Mutex::new(HashMap::new())),
        idx: Arc::new(Mutex::new(1)),
    };

    let router = Router::new()
        .route("/", get(home))
        .route("/stream", get(stream))
        .route("/styles.css", get(styles))
        .route("/todos", get(fetch_todos).post(create_todo))
        .route("/todos/:id", delete(delete_todo))
        .route("/todos/stream", get(handle_stream))
        .with_state(state)
        .layer(Extension(tx));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::Server::from_tcp(listener.into_std().unwrap())
        .unwrap()
        .serve(router.into_make_service())
        .await
        .unwrap();
}

async fn home() -> impl IntoResponse {
    HelloTemplate
}

async fn stream() -> impl IntoResponse {
    StreamTemplate
}

async fn fetch_todos(State(state): State<AppState>) -> impl IntoResponse {
    let todos = state
        .todos
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<Todo>>();

    Records { todos }
}

pub async fn styles() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/css")
        .body(include_str!("../templates/styles.css").to_owned())
        .unwrap()
}

async fn create_todo(
    State(state): State<AppState>,
    Extension(tx): Extension<TodosStream>,
    Form(form): Form<TodoNew>,
) -> impl IntoResponse {
    let mut todos = state.todos.lock().unwrap();
    let mut idx = state.idx.lock().unwrap();
    *idx += 1;
    let id = idx.clone();
    let todo = Todo {
        id,
        description: form.description.clone(),
    };
    todos.insert(id as i32, todo.clone());

    if tx
        .send(TodoUpdate {
            mutation_kind: MutationKind::Create,
            id,
        })
        .is_err()
    {
        eprintln!(
            "Record with ID {} was created but nobody's listening to the stream!",
            id
        );
    }

    TodoNewTemplate { todo }
}

async fn delete_todo(
    State(state): State<AppState>,
    Path(id): Path<i32>,
    Extension(tx): Extension<TodosStream>,
) -> impl IntoResponse {
    let mut todos = state.todos.lock().unwrap();
    todos.remove(&id);

    if tx
        .send(TodoUpdate {
            mutation_kind: MutationKind::Delete,
            id,
        })
        .is_err()
    {
        eprintln!(
            "Record with ID {} was deleted but nobody's listening to the stream!",
            id
        );
    }

    StatusCode::OK
}

#[derive(Template)]
#[template(path = "index.html")]
struct HelloTemplate;

#[derive(Template)]
#[template(path = "stream.html")]
struct StreamTemplate;

#[derive(Template)]
#[template(path = "todos.html")]
struct Records {
    todos: Vec<Todo>,
}

#[derive(Template)]
#[template(path = "todo.html")]
struct TodoNewTemplate {
    todo: Todo,
}

pub async fn handle_stream(
    Extension(tx): Extension<TodosStream>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = tx.subscribe();

    let stream = BroadcastStream::new(rx);

    Sse::new(
        stream
            .map(|msg| {
                let msg = msg.unwrap();
                let json = format!("<div>{}</div>", json!(msg));
                Event::default().data(json) /*.event(match msg.mutation_kind {
                                                MutationKind::Create => "create",
                                                MutationKind::Delete => "delete",
                                            })*/
            })
            .map(Ok),
    )
    .keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(600))
            .text("keep-alive-text"),
    )
}
