use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Sse},
    routing::{get, post},
    Form, Router, ServiceExt,
};
use itertools::*;
use maud::{html, Markup, DOCTYPE};
use process::ElectionProcess;
use qrcode::{render::svg::Color, QrCode};
use rand::distributions::DistString;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    convert::Infallible,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast::Sender;
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use tower::Layer;
use tower_http::{normalize_path::NormalizePathLayer, services::ServeDir, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::process::ElectionPhase;

mod process;

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // axum logs rejections from built-in extractors with the `axum::rejection`
                // target, at `TRACE` level. `axum::rejection=trace` enables showing those events
                "example_tracing_aka_logging=info,tower_http=debug,axum::rejection=trace".into()
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
        process::ElectionProcess::new(
            "1337".to_string(),
            process::ElectionPhase::FirstVote,
            "Test Role".to_string(),
            test_nominee_map,
            test_vote_map,
            HashMap::new(),
        ),
    );

    let mut streams = HashMap::new();
    streams.insert("1337".to_string(), tokio::sync::broadcast::channel(16).0);

    let router = Router::new()
        .route("/", get(view_home))
        .route("/election", post(post_election))
        .route("/election/join", get(get_election_join))
        .route("/election/:id/voting", get(view_election_voting))
        .route("/election/:id/voting", post(post_election_voting))
        .route("/election/:id/voting/form", get(get_election_voting_form))
        .route("/election/:id/eval", get(view_election_eval))
        .route("/election/:id/eval/content", get(get_election_eval_content))
        .route("/election/:id/step/:type/:step", post(post_election_step))
        .route("/election/:id/stream", get(get_election_sse_stream))
        .with_state(ElectionDB {
            db: Arc::new(Mutex::new(state)),
            streams: Arc::new(Mutex::new(streams)),
            base_url: std::env::var("BASE_URL").unwrap_or("http://localhost:3000".to_string()),
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

type StatusError = (StatusCode, &'static str);
const DB_UNLOCK_ERR: StatusError = (StatusCode::INTERNAL_SERVER_ERROR, "DB Lock error");

#[derive(Clone)]
struct ElectionDB {
    db: Arc<Mutex<HashMap<String, ElectionProcess>>>,
    streams: Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<ElectionUpdate>>>>,
    base_url: String,
}

impl ElectionDB {
    fn create_new_election(
        &self,
        elected_role: &str,
        nominees: Vec<&str>,
    ) -> Result<String, StatusError> {
        let mut db = self.db.lock().map_err(|_| DB_UNLOCK_ERR)?;

        let id = rand::distributions::Alphanumeric
            .sample_string(&mut rand::thread_rng(), 5)
            .to_ascii_lowercase();

        let election = ElectionProcess::new_and_cleaned(id.clone(), elected_role, nominees);

        db.insert(id.clone(), election);

        self.streams
            .lock()
            .unwrap()
            .insert(id.clone(), tokio::sync::broadcast::channel(16).0);

        Ok(id)
    }

    fn modify_election<F, T>(&self, id: &str, f: F) -> Result<T, StatusError>
    where
        F: FnOnce(&mut ElectionProcess, Sender<ElectionUpdate>) -> Result<T, StatusError>,
    {
        let mut db = self.db.lock().map_err(|_| DB_UNLOCK_ERR)?;
        db.get_mut(id)
            .ok_or((StatusCode::NOT_FOUND, "Election not found"))
            .and_then(|election| {
                let stream = self
                    .streams
                    .lock()
                    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Stream Lock error"))?
                    .get(id)
                    .ok_or((StatusCode::NOT_FOUND, "Stream not found"))?
                    .clone();
                f(election, stream)
            })
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct ElectionCreation {
    elected_role: String,
    nominees: String,
}

async fn post_election(
    State(state): State<ElectionDB>,
    Form(form): Form<ElectionCreation>,
) -> Result<impl IntoResponse, StatusError> {
    let election_id = state.create_new_election(
        form.elected_role.as_str(),
        form.nominees.lines().collect_vec(),
    )?;

    let redirect_response = (
        StatusCode::CREATED,
        [("HX-Redirect", format!("/election/{}/eval", election_id))],
    );
    Ok(redirect_response.into_response())
}

async fn post_election_step(
    Path((id, step_type, step)): Path<(String, String, String)>,
    State(state): State<ElectionDB>,
) -> Result<impl IntoResponse, StatusError> {
    state.modify_election(&id, |election, stream| {
        let src_phase = &process::ElectionPhase::from_str(&step)
            .map_err(|_e| (StatusCode::BAD_REQUEST, "Step unknown"))?;

        if election.phase().eq(src_phase) {
            match &step_type[..] {
                "next" => {
                    election.step_next();
                }
                "prev" => {
                    election.step_prev();
                }
                "reset" => {
                    election.reset_votes();
                }
                _ => Err((StatusCode::BAD_REQUEST, "Invalid step type"))?,
            };

            stream
                .send(ElectionUpdate::PhaseChanged)
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Stream send error"))?;

            Ok((StatusCode::ACCEPTED, [("HX-Refresh", "true")]).into_response())
        } else {
            Ok((StatusCode::BAD_REQUEST, [("HX-Refresh", "true")]).into_response())
        }
    })
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

    election.add_vote(form.voter_name, form.vote);

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

async fn get_election_join(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
    let id = params.get("election_id");
    match id {
        Some(id) => (
            StatusCode::OK,
            [(
                "HX-Redirect".to_string(),
                format!("/election/{}/voting", id),
            )],
        ),
        None => (
            StatusCode::BAD_REQUEST,
            [("HX-Redirect".to_string(), "/".to_string())],
        ),
    }
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
    let voting_path = format!("/election/{}/voting", id);
    let voting_url = format!("{}{}", &state.base_url, voting_path);
    let qrcode_svg = QrCode::with_error_correction_level(voting_url.as_bytes(), qrcode::EcLevel::H)
        .unwrap()
        .render::<Color>()
        .quiet_zone(true)
        .dark_color(Color("var(--qr-bg)"))
        .light_color(Color("var(--qr-fg)"))
        .build();

    Ok(base_html(
        format!("{} - Evaluation", election.elected_role()).as_str(),
        html! { (election.elected_role()) br; "Evaluation" },
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
        html!(
            dialog #"share-dialog" style="text-align: center;" {
                article {
                    header {
                        h2 { "Share the election of " (election.elected_role()) }
                    }
                    (maud::PreEscaped(qrcode_svg))
                    br; br;
                    a ."contrast" href=(voting_path) { (voting_url) }
                    footer {
                        button style="margin-right:unset;"
                            onclick="document.getElementById('share-dialog').close()" { "Close" }
                    }
                }
            }
            button ."secondary" onclick="document.getElementById('share-dialog').show()"
                style="transform: translate(0,0.2em)" {
                (id) " 🔗"
            }
        ),
    ))
}

fn eval_election(election: &ElectionProcess) -> Markup {
    let buttons = html! {
        div ."button-grid" {
            button ."lbut" disabled[election.phase() == ElectionPhase::FirstVote]
            hx-post={"/election/" (election.id().to_string()) "/step/prev/" (election.phase().to_string())}
            hx-trigger="click" hx-swap="none" hx-confirm="Are you sure?" {
                "Previous Phase"
            }

            button ."cbut secondary"
            disabled[election.phase() != ElectionPhase::FirstVote && election.phase() != ElectionPhase::SecondVote]
            hx-post={"/election/" (election.id().to_string()) "/step/reset/" (election.phase().to_string())}
            hx-trigger="click" hx-swap="none" hx-confirm="Are you sure?" {
                "Reset Votes"
            }

            button ."rbut" disabled[election.phase() == ElectionPhase::SafetyRound]
            hx-post={"/election/" (election.id().to_string()) "/step/next/" (election.phase().to_string())}
            hx-trigger="click" hx-swap="none" hx-confirm="Are you sure?" {
                "Next Phase"
            }
        }
    };

    if election.phase() == process::ElectionPhase::SafetyRound {
        let accumulated_votes = election.accumulated_votes();
        let all_with_max_votes = accumulated_votes.all_with_max_votes();

        return html! {
            h2 { (election.phase().nice_title()) }
            p { (election.phase().nice_description()) }
            p { "The most votes were for: " ( all_with_max_votes.join(", ") ) }

            {( buttons )}
        };
    }

    let tally = eval_tally(election);

    let eval_count = {
        match election.phase() {
            process::ElectionPhase::FirstVote
            | process::ElectionPhase::FirstTally
            | process::ElectionPhase::SecondVote
            | process::ElectionPhase::SecondTally => {
                html! { p { "Number of votes: " (election.vote_count()) } }
            }
            process::ElectionPhase::SafetyRound => unreachable!(),
        }
    };

    html! {
        h2 { (election.phase().nice_title()) }

        {( eval_count )}

        {( tally )}

        {( buttons )}
    }
}

fn eval_tally(election: &ElectionProcess) -> Markup {
    if !(election.phase() == process::ElectionPhase::FirstTally
        || election.phase() == process::ElectionPhase::SecondTally)
    {
        return html! {
            p { "The following users have voted:" }
            ul #"voter-list" {
                @for voter_name in election.voters() {
                    li { (voter_name) }
                }
            }
        };
    }

    let round = election.current_round();
    let accumulated_votes = election.accumulated_votes();
    let max_votes = accumulated_votes.max_votes();

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
                            td { (election.get_vote(*vote)) }
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
                    @for (votee, vote_count) in accumulated_votes.results {
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
        election.elected_role(),
        html! {(election.elected_role())},
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
        html!(strong { a href={"/election/" (id) "/voting"} ."secondary" {(id)} }),
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
    match election.phase() {
        process::ElectionPhase::FirstVote | process::ElectionPhase::SecondVote => {
            let sorted_nominees = election.get_sorted_nominees();
            html! {
                h2 { (election.phase().nice_title()) }
                p { (election.phase().nice_description()) }
                form #"vote" ."table rows" {
                    label for="elected_role" {
                        "Voter Name: ";
                        input type="text" name="voter_name" required {}
                    }
                    label for="vote" {
                        "Vote :";
                        select name="vote" required {
                            @for (id, nominee) in sorted_nominees {
                                option value=(id) { (nominee) }
                            }
                        }
                    }
                    button
                      hx-post={"/election/" (election.id()) "/voting"}
                      hx-trigger="click" hx-target="#vote" hx-swap="outerHTML"
                      style="left: 50%; position: relative; translate: -50%;" {
                        "Vote!"
                    }
                }
            }
        }
        process::ElectionPhase::FirstTally | process::ElectionPhase::SecondTally => {
            html! {
                h2 { (election.phase().nice_title()) }
                p { (election.phase().nice_description()) }
                {( eval_tally(election) )}
            }
        }
        process::ElectionPhase::SafetyRound => {
            let accumulated_votes = election.accumulated_votes();
            let all_with_max_votes = accumulated_votes.all_with_max_votes();
            html!(
                h2 { (election.phase().nice_title()) }
                p { (election.phase().nice_description()) }
                p { "The most votes were for: " ( all_with_max_votes.join(", ") ) }
            )
        }
    }
}

async fn view_home() -> Markup {
    base_html(
        "IEP Tool Home",
        html!("IEP Tool Home"),
        html! {
            p { "Welcome to the Integrative Election Process Tool! Press the button below to start a new election." }

            h2 { "Join Election" }
            form #"join-election" ."table rows" {
                label for="election_id" {
                    "Election ID: ";
                    input type="text" name="election_id" required {}
                }
                button
                  hx-get={"/election/join"}
                  hx-trigger="click" hx-swap="outerHTML"
                  hx-include="[name='election_id']"
                  style="left: 50%; position: relative; translate: -50%;" {
                    "Join Election"
                }
            }


            br;
            h2 { "New Election" }

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
                script src="/htmx.1.9.12.js" {}
                script src="/htmx-ext-sse.1.9.12.js" {}
                //script src="https://unpkg.com/htmx.org" {}
                //script src="https://unpkg.com/htmx.org/dist/ext/sse.js" {}
                //link rel="stylesheet" href="https://unpkg.com/missing.css@1.1.1" {}
                link
                  rel="stylesheet"
                  href="/pico.min.css" {}
                link rel="stylesheet" href="/charts.min.css" {}
                link rel="stylesheet" href="/styles.css" {}
                title { "IEP - " (title) }
            }
            body {
                header ."container" {
                    nav {
                        ul { li { a href="/" ."secondary" style="font-size: 1.5em;" {"🏠"} } }
                        ul { li style="font-size: 1.5em; text-align: center;"{ strong {(title_markup)} }}
                        ul { li {(fragment)} }
                    }
                }
                main ."container" {
                    div { (content) }
                }
                footer ."container" {
                    p { "IEP Tool v" (env!("CARGO_PKG_VERSION"))}
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
