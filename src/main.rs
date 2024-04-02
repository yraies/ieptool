use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Sse},
    routing::{get, post},
    Form, Router, ServiceExt,
};
use itertools::*;
use maud::{html, Markup, DOCTYPE};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use tower::Layer;
use tower_http::{normalize_path::NormalizePathLayer, services::ServeDir, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // axum logs rejections from built-in extractors with the `axum::rejection`
                // target, at `TRACE` level. `axum::rejection=trace` enables showing those events
                "example_tracing_aka_logging=info,tower_http=info,axum::rejection=trace".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let mut state = HashMap::new();
    let mut test_nominee_map = HashMap::new();
    test_nominee_map.insert(13589, "Test Nominee 1".to_string());
    test_nominee_map.insert(29852, "Test2".to_string());
    test_nominee_map.insert(96109, "Test Nominee 3".to_string());
    let mut test_vote_map = HashMap::new();
    test_vote_map.insert("Test Voter 1".to_string(), 13589);
    test_vote_map.insert("Test Voter 2".to_string(), 29852);
    test_vote_map.insert("Test Voter 3".to_string(), 13589);
    state.insert(
        1337,
        ElectionProcess {
            id: 1337,
            phase: ElectionPhase::FirstVote,
            elected_role: "Test Role".to_string(),
            nominees: test_nominee_map,
            first_round_id: test_vote_map,
            second_round_id: HashMap::new(),
        },
    );

    let mut streams = HashMap::new();
    streams.insert(1337, tokio::sync::broadcast::channel(16).0);

    let router = Router::new()
        .route("/", get(home))
        .route("/election", post(create_election))
        .route("/election/:id", get(view_election))
        .route("/election/:id/votes/form", get(voting_form))
        .route("/election/:id/votes", get(view_votes))
        .route("/election/:id/votes/count", get(view_vote_count))
        .route("/election/:id/votes", post(add_vote))
        .route("/election/:id/next-step/:step", post(step_election))
        .route("/election/:id/stream", get(get_sse_stream))
        .with_state(ElectionDB {
            db: Arc::new(Mutex::new(state)),
            streams: Arc::new(Mutex::new(streams)),
        })
        .fallback_service(ServeDir::new("static"))
        .layer(TraceLayer::new_for_http());
    let router = NormalizePathLayer::trim_trailing_slash().layer(router);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, ServiceExt::<Request>::into_make_service(router))
        .await
        .unwrap();
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Copy, Clone)]
enum ElectionPhase {
    FirstVote,
    FirstTally,
    SecondVote,
    SecondTally,
}

impl ElectionPhase {
    fn next_url(&self) -> &'static str {
        match self {
            ElectionPhase::FirstVote => "tally1",
            ElectionPhase::FirstTally => "vote2",
            ElectionPhase::SecondVote => "tally2",
            ElectionPhase::SecondTally => "none",
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct ElectionProcess {
    id: u64,
    phase: ElectionPhase,
    elected_role: String,
    nominees: HashMap<u64, String>,
    first_round_id: HashMap<String, u64>,
    second_round_id: HashMap<String, u64>,
}

#[derive(Clone)]
struct ElectionDB {
    db: Arc<Mutex<HashMap<u64, ElectionProcess>>>,
    streams: Arc<Mutex<HashMap<u64, tokio::sync::broadcast::Sender<ElectionUpdate>>>>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct ElectionCreation {
    elected_role: String,
    nominees: String,
}

async fn create_election(
    State(state): State<ElectionDB>,
    Form(form): Form<ElectionCreation>,
) -> impl IntoResponse {
    let mut db = state.db.lock().unwrap();
    let id = rand::random::<u64>();
    let nominees = form
        .nominees
        .lines()
        .enumerate()
        .map(|(i, n)| (i as u64, n.to_string()))
        .collect::<HashMap<_, _>>();
    let election = ElectionProcess {
        id,
        phase: ElectionPhase::FirstVote,
        elected_role: form.elected_role,
        nominees,
        first_round_id: HashMap::new(),
        second_round_id: HashMap::new(),
    };
    db.insert(id, election);

    state
        .streams
        .lock()
        .unwrap()
        .insert(id, tokio::sync::broadcast::channel(16).0);

    (
        StatusCode::CREATED,
        [("HX-Redirect", format!("/election/{}", id))],
    )
        .into_response()
}

async fn step_election(
    Path((id, step)): Path<(u64, String)>,
    State(state): State<ElectionDB>,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    let mut db = state
        .db
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "DB Lock error"))?;
    let election = db
        .get_mut(&id)
        .ok_or((StatusCode::NOT_FOUND, "Election not found"))?;
    let stream = state
        .streams
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Stream Lock error"))?
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "Stream not found"))?
        .clone();

    Ok(match (election.phase, &step[..]) {
        (ElectionPhase::FirstVote, "tally1") => {
            election.phase = ElectionPhase::FirstTally;
            stream.send(ElectionUpdate::PhaseChanged).unwrap(); // todo handle error
            (StatusCode::ACCEPTED, [("HX-Refresh", "true")])
        }
        (ElectionPhase::FirstTally, "vote2") => {
            election.phase = ElectionPhase::SecondVote;
            stream.send(ElectionUpdate::PhaseChanged).unwrap();
            (StatusCode::ACCEPTED, [("HX-Refresh", "true")])
        }
        (ElectionPhase::SecondVote, "tally2") => {
            election.phase = ElectionPhase::SecondTally;
            stream.send(ElectionUpdate::PhaseChanged).unwrap();
            (StatusCode::ACCEPTED, [("HX-Refresh", "true")])
        }
        _ => (StatusCode::BAD_REQUEST, [("HX-Refresh", "true")]),
    }
    .into_response())
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Vote {
    voter_name: String,
    vote: u64,
}

async fn add_vote(
    State(state): State<ElectionDB>,
    Path(id): Path<u64>,
    Form(form): Form<Vote>,
) -> Result<Markup, (StatusCode, &'static str)> {
    let mut db = state
        .db
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "DB Lock error"))?;
    let election = db
        .get_mut(&id)
        .ok_or((StatusCode::NOT_FOUND, "Election not found"))?;
    match election.phase {
        ElectionPhase::FirstVote => {
            election.first_round_id.insert(form.voter_name, form.vote);
        }
        ElectionPhase::SecondVote => {
            election.second_round_id.insert(form.voter_name, form.vote);
        }
        _ => {}
    }
    state
        .streams
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Stream Lock error"))?
        .get(&id)
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "Stream not found"))?
        .send(ElectionUpdate::VotesChanged)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Stream send error"))?;

    Ok(html! {
        p { "Vote added!" }
    })
}

async fn view_vote_count(
    Path(id): Path<u64>,
    State(state): State<ElectionDB>,
) -> Result<Markup, StatusCode> {
    let db = state
        .db
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let election = db.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    Ok(html! {
        @if election.phase == ElectionPhase::FirstVote || election.phase == ElectionPhase::FirstTally {
            p { "Number of votes: " (election.first_round_id.len()) }
        } @else if election.phase == ElectionPhase::SecondVote || election.phase == ElectionPhase::SecondTally {
            p { "Number of votes: " (election.second_round_id.len()) }
        }
    })
}

async fn view_votes(
    Path(id): Path<u64>,
    State(state): State<ElectionDB>,
) -> Result<Markup, StatusCode> {
    let db = state
        .db
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let election = db.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    Ok(base_html(
        format!("{} - Evaluation", election.elected_role).as_str(),
        html! {
            div hx-ext="sse" sse-connect={"/election/" (id.to_string()) "/stream"} {
                h2 { (format!("{:?}", election.phase).as_str()) }

                div #"vote-count" hx-get={"/election/" (id.to_string()) "/votes/count"} hx-trigger="load,sse:phase-changed,sse:votes-changed" hx-swap="innerHTML" {
                    "Loading..."
                }

                {( tally_votes(election) )}

                @if election.phase != ElectionPhase::SecondTally {
                    button hx-post={"/election/" (id.to_string()) "/next-step/" (election.phase.next_url())} hx-trigger="click" hx-swap="none" {
                        "Next Phase"
                    }
                }
            }
        },
    ))
}

fn tally_votes(election: &ElectionProcess) -> Markup {
    let round = match election.phase {
        ElectionPhase::FirstVote => &election.first_round_id,
        ElectionPhase::FirstTally => &election.first_round_id,
        ElectionPhase::SecondVote => &election.second_round_id,
        ElectionPhase::SecondTally => &election.second_round_id,
    };

    html! {
        @if election.phase == ElectionPhase::FirstTally || election.phase == ElectionPhase::SecondTally {
            h3 { "Votes" }

            table ."table" {
                thead {
                    tr {
                        th { "Voter" }
                        th { "Vote" }
                    }
                }
                tbody {
                    @for (voter_name, vote) in round {
                        tr {
                            td { (voter_name) }
                            td { (election.nominees.get(vote).unwrap()) }
                        }
                    }
                }
            }

            h3 { "Results" }

            table ."table" {
                thead {
                    tr {
                        th { "Nominee" }
                        th { "Votes" }
                    }
                }
                tbody {
                    @let grouped = round.iter().into_group_map_by(|(_, &v)| v);
                    @let sorted = grouped.iter().map(|(k, v)| (k, v.len())).filter(|(_k,v)| *v > 0).sorted_by_key(|(_, v)| *v).rev().collect::<Vec<_>>();
                    @for group in sorted {
                        tr {
                            td { (election.nominees.get(group.0).unwrap()) }
                            td { (group.1) }
                        }
                    }
                }
            }
        }
    }
}

async fn view_election(
    Path(id): Path<u64>,
    State(state): State<ElectionDB>,
) -> Result<Markup, StatusCode> {
    let db = state
        .db
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let election = db.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    Ok(base_html(
        election.elected_role.as_str(),
        html! {
            div hx-ext="sse" sse-connect={"/election/" (id.to_string()) "/stream"} {
              div #"vote-content" hx-get={"/election/" (id.to_string()) "/votes/form"} hx-trigger="load,sse:phase-changed" hx-swap="innerHTML" { "Loading ..." }
            }
        },
    ))
}

async fn voting_form(
    Path(id): Path<u64>,
    State(state): State<ElectionDB>,
) -> Result<Markup, StatusCode> {
    let db = state
        .db
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let election = db.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    if election.phase != ElectionPhase::FirstVote && election.phase != ElectionPhase::SecondVote {
        return Ok(html! {
            h2 { (format!("{:?}", election.phase).as_str()) }
            p #"vote" { "Voting is not allowed at this time." }
        });
    } else {
        Ok(html! {
            h2 { (format!("{:?}", election.phase).as_str()) }
            form #"vote" ."table rows" {
                label for="elected_role" {
                    "Voter Name: ";
                    input type="text" name="voter_name" required {}
                }
                label for="vote" {
                    "Vote :";
                    select name="vote" required {
                        @for (id, nominee) in &election.nominees {
                            option value=(id.to_string()) { (nominee) }
                        }
                    }
                }
                label {
                    "Done? ";
                    button
                      hx-post={"/election/" (election.id.to_string()) "/votes"}
                      hx-trigger="click" hx-target="#vote" hx-swap="outerHTML" {
                        "Vote!"
                    }
                }
            }
        })
    }
}

async fn home() -> Markup {
    base_html(
        "Home",
        html! {
            p { "Welcome to the Integrative Election Process Tool! Press the button below to start a new election." }
            form #"new-election" ."table rows" {
                label for="elected_role" {
                    "Elected Role: ";
                    input type="text" name="elected_role" required {}
                }
                label for="nominees" {
                    "Nominees :";
                    textarea name="nominees" placeholder="one nominee per line" required {}
                }
                label {
                    "Done? ";
                    button hx-post="/election" hx-trigger="click" hx-swap="none" {
                        "Start Election"
                    }
                }
            }
        },
    )
}

fn base_html(title: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="UTF-8" {}
                meta name="viewport" content="width=device-width, initial-scale=1, shrink-to-fit=no" {}
                script src="https://unpkg.com/htmx.org" {}
                script src="https://unpkg.com/htmx.org/dist/ext/sse.js" {}
                link rel="stylesheet" href="https://unpkg.com/missing.css@1.1.1" {}
                title { "IEP - " (title) }
            }
            body {
                main {
                    h1 { (title) }
                    div { (content) }
                }
            }
        }
    }
}

#[derive(Debug, Serialize, Clone, Copy)]
enum ElectionUpdate {
    VotesChanged,
    PhaseChanged,
}

async fn get_sse_stream(
    Path(id): Path<u64>,
    State(state): State<ElectionDB>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, &'static str)> {
    let rx = state
        .streams
        .lock()
        .map_err(|_e| (StatusCode::INTERNAL_SERVER_ERROR, "Lock error"))?
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "Election stream not found"))?
        .subscribe();

    let stream = BroadcastStream::new(rx);

    Ok(Sse::new(
        stream
            .map(|msg| {
                let event = match msg.unwrap() {
                    ElectionUpdate::VotesChanged => "votes-changed",
                    ElectionUpdate::PhaseChanged => "phase-changed",
                };
                Event::default().event(event).data(event)
            })
            .map(Ok),
    )
    .keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(600))
            .text("keep-alive-text"),
    ))
}
