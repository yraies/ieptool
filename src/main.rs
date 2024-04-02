use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Sse},
    routing::{get, post},
    Form, Router, ServiceExt,
};
use itertools::*;
use maud::{html, Markup, DOCTYPE};
use rand::distributions::DistString;
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
        "1337".to_string(),
        ElectionProcess {
            id: "1337".to_string(),
            phase: ElectionPhase::FirstVote,
            elected_role: "Test Role".to_string(),
            nominees: test_nominee_map,
            first_round_id: test_vote_map,
            second_round_id: HashMap::new(),
        },
    );

    let mut streams = HashMap::new();
    streams.insert("1337".to_string(), tokio::sync::broadcast::channel(16).0);

    let router = Router::new()
        .route("/", get(view_home))
        .route("/election", post(post_election))
        .route("/election/:id/voting", get(view_election_voting))
        .route("/election/:id/voting", post(post_election_voting))
        .route("/election/:id/voting/form", get(get_election_voting_form))
        .route("/election/:id/eval", get(view_election_eval))
        .route("/election/:id/eval/content", get(get_election_eval_content))
        .route("/election/:id/next-step/:step", post(post_election_step))
        .route("/election/:id/stream", get(get_election_sse_stream))
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

    fn nice_title(&self) -> &'static str {
        match self {
            ElectionPhase::FirstVote => "First Vote",
            ElectionPhase::FirstTally => "Results of First Vote",
            ElectionPhase::SecondVote => "Second Vote",
            ElectionPhase::SecondTally => "Results of Second Vote",
        }
    }

    fn nice_description(&self) -> Markup {
        match self {
            ElectionPhase::FirstVote => html!(p {"Please vote for your preferred candidate."}),
            ElectionPhase::FirstTally => {
                html!(
                    p {"The results of the first vote are in!"}
                    p {"Everyone can now explain their vote."}
                )
            }
            ElectionPhase::SecondVote => html!(p {"Please vote for your preferred candidate."}),
            ElectionPhase::SecondTally => html!(p {"The results of the second vote are in!"}),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct ElectionProcess {
    id: String,
    phase: ElectionPhase,
    elected_role: String,
    nominees: HashMap<u64, String>,
    first_round_id: HashMap<String, u64>,
    second_round_id: HashMap<String, u64>,
}

#[derive(Clone)]
struct ElectionDB {
    db: Arc<Mutex<HashMap<String, ElectionProcess>>>,
    streams: Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<ElectionUpdate>>>>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct ElectionCreation {
    elected_role: String,
    nominees: String,
}

async fn post_election(
    State(state): State<ElectionDB>,
    Form(form): Form<ElectionCreation>,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    let mut db = state
        .db
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "DB Lock error"))?;
    let id = rand::distributions::Alphanumeric.sample_string(&mut rand::thread_rng(), 4);
    let nominees = form
        .nominees
        .lines()
        .filter(|n| !n.is_empty())
        .sorted()
        .enumerate()
        .map(|(i, n)| (i as u64, n.to_string()))
        .collect::<HashMap<_, _>>();
    let election = ElectionProcess {
        id: id.clone(),
        phase: ElectionPhase::FirstVote,
        elected_role: form.elected_role,
        nominees,
        first_round_id: HashMap::new(),
        second_round_id: HashMap::new(),
    };
    db.insert(id.clone(), election);

    state
        .streams
        .lock()
        .unwrap()
        .insert(id.clone(), tokio::sync::broadcast::channel(16).0);

    Ok((
        StatusCode::CREATED,
        [("HX-Redirect", format!("/election/{}/eval", id))],
    )
        .into_response())
}

async fn post_election_step(
    Path((id, step)): Path<(String, String)>,
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

    if election.phase.next_url().eq(&step[..]) {
        election.phase = match election.phase {
            ElectionPhase::FirstVote => ElectionPhase::FirstTally,
            ElectionPhase::FirstTally => ElectionPhase::SecondVote,
            ElectionPhase::SecondVote => ElectionPhase::SecondTally,
            ElectionPhase::SecondTally => ElectionPhase::SecondTally,
        };
        stream
            .send(ElectionUpdate::PhaseChanged)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Stream send error"))?;
        Ok((StatusCode::ACCEPTED, [("HX-Refresh", "true")]).into_response())
    } else {
        Ok((StatusCode::BAD_REQUEST, [("HX-Refresh", "true")]).into_response())
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Vote {
    voter_name: String,
    vote: u64,
}

async fn post_election_voting(
    State(state): State<ElectionDB>,
    Path(id): Path<String>,
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

async fn get_election_eval_content(
    Path(id): Path<String>,
    State(state): State<ElectionDB>,
) -> Result<Markup, StatusCode> {
    let db = state
        .db
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let election = db.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    Ok(eval_election(election))
}

async fn view_election_eval(
    Path(id): Path<String>,
    State(state): State<ElectionDB>,
) -> Result<Markup, StatusCode> {
    let db = state
        .db
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let election = db.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    Ok(base_html(
        format!("{} - Evaluation", election.elected_role).as_str(),
        html! { (election.elected_role) br; "Evaluation" },
        html! {
            div hx-ext="sse" sse-connect={"/election/" (id.to_string()) "/stream"} {
                div #"eval"
                  hx-get={"/election/" (id.to_string()) "/eval/content"}
                  hx-trigger="sse:phase-changed,sse:votes-changed"
                  hx-swap="innerHTML" {
                    {(eval_election(election))}
                }
            }
        },
        html!(strong {a href={"/election/" (id) "/voting"} ."secondary" {(id)}}),
    ))
}

fn eval_election(election: &ElectionProcess) -> Markup {
    let tally = eval_tally(election);

    let eval_count = {
        match election.phase {
            ElectionPhase::FirstVote | ElectionPhase::FirstTally => {
                html! { p { "Number of votes: " (election.first_round_id.len()) } }
            }
            ElectionPhase::SecondVote | ElectionPhase::SecondTally => {
                html! { p { "Number of votes: " (election.second_round_id.len()) } }
            }
        }
    };

    html! {
        h2 { (election.phase.nice_title()) }

        {( eval_count )}

        {( tally )}

        @if election.phase != ElectionPhase::SecondTally {
            button
              hx-post={"/election/" (election.id.to_string()) "/next-step/" (election.phase.next_url())}
              hx-trigger="click" hx-swap="none" hx-confirm="Are you sure?"
              style="left: 50%; position: relative; translate: -50%;" {
                "Next Phase"
            }
        }
    }
}

fn eval_tally(election: &ElectionProcess) -> Markup {
    let round = match election.phase {
        ElectionPhase::FirstVote => &election.first_round_id,
        ElectionPhase::FirstTally => &election.first_round_id,
        ElectionPhase::SecondVote => &election.second_round_id,
        ElectionPhase::SecondTally => &election.second_round_id,
    };

    if !(election.phase == ElectionPhase::FirstTally
        || election.phase == ElectionPhase::SecondTally)
    {
        return html! {};
    }

    let accumulated_votes = round
        .iter()
        .into_group_map_by(|(_, &v)| v)
        .iter()
        .map(|(k, v)| (election.nominees.get(k).unwrap(), v.len()))
        .filter(|(_k, v)| *v > 0)
        .sorted_by(|a, b| Ord::cmp(&a.1, &b.1).then_with(|| Ord::cmp(&a.0, &b.0).reverse()))
        .rev()
        .collect::<Vec<_>>();
    let max_votes = accumulated_votes
        .iter()
        .map(|(_k, v)| *v)
        .max()
        .unwrap_or(1);

    html! {
        br;
        details open {
            summary { "Individual Votes" }
            table ."striped" {
                thead {
                    tr {
                        th { "Voter" }
                        th { "Vote" }
                    }
                }
                tbody {
                    @for (voter_name, vote) in round.iter().sorted_by_key(|(n, _)| &n[..]) {
                        tr {
                            td { (voter_name) }
                            td { (election.nominees.get(vote).unwrap()) }
                        }
                    }
                }
            }
        }
        br;
        div #"eval-chart" {
            table
                ."charts-css bar show-labels data-spacing-1 data-start show-data-on-hover"
                style="--labels-size: 10em;" {
                thead {
                    tr {
                        th { "Nominee" }
                        th { "Votes" }
                    }
                }
                tbody {
                    @for (votee, vote_count) in accumulated_votes {
                        tr {
                            th scope="row" {(votee)}
                            td style={"--size: " (vote_count as f32 / (max_votes as f32))}{
                                span ."data" {(vote_count)}
                            }
                        }
                    }
                }
            }
        }
        br;
    }
}

async fn view_election_voting(
    Path(id): Path<String>,
    State(state): State<ElectionDB>,
) -> Result<Markup, StatusCode> {
    let db = state
        .db
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let election = db.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    Ok(base_html(
        election.elected_role.as_str(),
        html! {(election.elected_role.as_str())},
        html! {
            div hx-ext="sse" sse-connect={"/election/" (id.to_string()) "/stream"} {
              div #"vote-content"
                hx-get={"/election/" (id.to_string()) "/voting/form"}
                hx-trigger="sse:phase-changed"
                hx-swap="innerHTML" {
                  ({ voting_form(election) })
              }
            }
        },
        html!(strong { a href={"/election/" (id) "/eval"} ."secondary" {(id)} }),
    ))
}

async fn get_election_voting_form(
    Path(id): Path<String>,
    State(state): State<ElectionDB>,
) -> Result<Markup, StatusCode> {
    let db = state
        .db
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let election = db.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(voting_form(election))
}

fn voting_form(election: &ElectionProcess) -> Markup {
    if election.phase != ElectionPhase::FirstVote && election.phase != ElectionPhase::SecondVote {
        html! {
            h2 { (election.phase.nice_title()) }
            p { (election.phase.nice_description()) }
            {( eval_tally(election) )}
        }
    } else {
        let sorted_nominees = election.nominees.iter().sorted_by_key(|(k, _)| *k);
        html! {
            h2 { (election.phase.nice_title()) }
            p { (election.phase.nice_description()) }
            form #"vote" ."table rows" {
                label for="elected_role" {
                    "Voter Name: ";
                    input type="text" name="voter_name" required {}
                }
                label for="vote" {
                    "Vote :";
                    select name="vote" required {
                        @for (id, nominee) in sorted_nominees {
                            option value=(id.to_string()) { (nominee) }
                        }
                    }
                }
                button
                  hx-post={"/election/" (election.id.to_string()) "/voting"}
                  hx-trigger="click" hx-target="#vote" hx-swap="outerHTML"
                  style="left: 50%; position: relative; translate: -50%;" {
                    "Vote!"
                }
            }
        }
    }
}

async fn view_home() -> Markup {
    base_html(
        "IEP Tool Home",
        html!("IEP Tool Home"),
        html! {
            p { "Welcome to the Integrative Election Process Tool! Press the button below to start a new election." }
            form #"new-election" ."table rows" {
                label for="elected_role" {
                    "Elected Role: ";
                    input type="text" name="elected_role" required {}
                }
                label for="nominees" {
                    "Nominees :";
                    textarea
                      name="nominees" placeholder="one nominee per line" required
                      style="min-height: 12em;" {}
                }
                button
                  hx-post="/election" hx-trigger="click" hx-swap="none"
                  style="left: 50%; position: relative; translate: -50%;" {
                    "Start Election"
                }
            }
        },
        html! {},
    )
}

fn base_html(title: &str, title_markup: Markup, content: Markup, fragment: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="UTF-8" {}
                meta name="viewport" content="width=device-width, initial-scale=1, shrink-to-fit=no" {}
                script src="https://unpkg.com/htmx.org" {}
                script src="https://unpkg.com/htmx.org/dist/ext/sse.js" {}
                //link rel="stylesheet" href="https://unpkg.com/missing.css@1.1.1" {}
                link
                  rel="stylesheet"
                  href="https://cdn.jsdelivr.net/npm/@picocss/pico@2/css/pico.min.css" {}
                link rel="stylesheet" href="https://unpkg.com/charts.css/dist/charts.min.css" {}
                link rel="stylesheet" href="/styles.css" {}
                title { "IEP - " (title) }
            }
            body {
                header ."container" {
                    nav {
                        ul { li { a href="/" ."secondary" {"🏠 IEP"} } }
                        ul { li style="font-size: 1.5em; text-align: center;"{ strong {(title_markup)} }}
                        ul { li {(fragment)} }
                    }
                }
                main ."container" {
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

async fn get_election_sse_stream(
    Path(id): Path<String>,
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
