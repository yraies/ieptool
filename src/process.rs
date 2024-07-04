use std::collections::HashMap;

use itertools::Itertools;
use maud::{html, Markup};
use serde::{Deserialize, Serialize};

#[derive(
    Serialize,
    Deserialize,
    PartialEq,
    Debug,
    Copy,
    Clone,
    strum_macros::EnumString,
    strum_macros::Display,
)]
pub(crate) enum ElectionPhase {
    FirstVote,
    FirstTally,
    SecondVote,
    SecondTally,
    SafetyRound,
}

impl ElectionPhase {
    pub(crate) fn nice_title(&self) -> &'static str {
        match self {
            ElectionPhase::FirstVote => "First Vote",
            ElectionPhase::FirstTally => "Results of First Vote",
            ElectionPhase::SecondVote => "Second Vote",
            ElectionPhase::SecondTally => "Results of Second Vote",
            ElectionPhase::SafetyRound => "Safety Round",
        }
    }

    pub(crate) fn nice_description(&self) -> Markup {
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
            ElectionPhase::SafetyRound => html!(
                p {"Is this decision safe enough to try?"}
            ),
        }
    }

    pub(crate) fn step_next(&self) -> ElectionPhase {
        match self {
            ElectionPhase::FirstVote => ElectionPhase::FirstTally,
            ElectionPhase::FirstTally => ElectionPhase::SecondVote,
            ElectionPhase::SecondVote => ElectionPhase::SecondTally,
            ElectionPhase::SecondTally => ElectionPhase::SafetyRound,
            ElectionPhase::SafetyRound => ElectionPhase::SafetyRound,
        }
    }

    pub(crate) fn step_prev(&self) -> ElectionPhase {
        match self {
            ElectionPhase::FirstVote => ElectionPhase::FirstVote,
            ElectionPhase::FirstTally => ElectionPhase::FirstVote,
            ElectionPhase::SecondVote => ElectionPhase::FirstTally,
            ElectionPhase::SecondTally => ElectionPhase::SecondVote,
            ElectionPhase::SafetyRound => ElectionPhase::SecondTally,
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub(crate) struct ElectionProcess {
    id: String,
    phase: ElectionPhase,
    elected_role: String,
    nominees: HashMap<u64, String>,
    first_round_id: HashMap<String, u64>,
    second_round_id: HashMap<String, u64>,
}

impl ElectionProcess {
    pub(crate) fn new(
        id: String,
        phase: ElectionPhase,
        elected_role: String,
        nominees: HashMap<u64, String>,
        first_round_id: HashMap<String, u64>,
        second_round_id: HashMap<String, u64>,
    ) -> Self {
        Self {
            id,
            phase,
            elected_role,
            nominees,
            first_round_id,
            second_round_id,
        }
    }

    pub(crate) fn new_and_cleaned(
        id: impl Into<String>,
        elected_role: impl Into<String>,
        nominees: Vec<&str>,
    ) -> Self {
        let nominees = nominees
            .into_iter()
            .filter(|n| !n.is_empty())
            .sorted()
            .dedup()
            .enumerate()
            .map(|(i, n)| (i as u64, n.to_string()))
            .collect::<HashMap<_, _>>();
        ElectionProcess {
            id: id.into(),
            phase: ElectionPhase::FirstVote,
            elected_role: elected_role.into(),
            nominees,
            first_round_id: HashMap::new(),
            second_round_id: HashMap::new(),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn elected_role(&self) -> &str {
        &self.elected_role
    }

    pub(crate) fn phase(&self) -> ElectionPhase {
        self.phase
    }

    pub(crate) fn step_next(&mut self) {
        self.phase = self.phase.step_next();
    }

    pub(crate) fn step_prev(&mut self) {
        self.phase = self.phase.step_prev();
    }

    pub(crate) fn reset_votes(&mut self) {
        if self.phase == ElectionPhase::FirstVote {
            self.first_round_id.clear();
        } else if self.phase == ElectionPhase::SecondVote {
            self.second_round_id.clear();
        }
    }

    pub(crate) fn add_vote(&mut self, voter_name: String, vote: u64) {
        match self.phase() {
            ElectionPhase::FirstVote => {
                self.first_round_id.insert(voter_name, vote);
            }
            ElectionPhase::SecondVote => {
                self.second_round_id.insert(voter_name, vote);
            }
            _ => {}
        }
    }

    pub(crate) fn get_vote(&self, vote: u64) -> &str {
        self.nominees.get(&vote).unwrap()
    }

    pub(crate) fn get_sorted_nominees(&self) -> Vec<(u64, String)> {
        self.nominees
            .iter()
            .sorted_by_key(|(k, _)| *k)
            .map(|(v, n)| (*v, n.to_owned()))
            .collect_vec()
    }

    pub(crate) fn vote_count(&self) -> usize {
        match self.phase() {
            ElectionPhase::FirstVote | ElectionPhase::FirstTally => self.first_round_id.len(),
            ElectionPhase::SecondVote | ElectionPhase::SecondTally => self.second_round_id.len(),
            ElectionPhase::SafetyRound => 0,
        }
    }

    pub(crate) fn current_round(&self) -> &HashMap<String, u64> {
        match self.phase() {
            ElectionPhase::FirstVote => &self.first_round_id,
            ElectionPhase::FirstTally => &self.first_round_id,
            ElectionPhase::SecondVote => &self.second_round_id,
            ElectionPhase::SecondTally => &self.second_round_id,
            ElectionPhase::SafetyRound => &self.second_round_id,
        }
    }

    pub(crate) fn voters(&self) -> Vec<&str> {
        let round = self.current_round();

        round.keys().map(|s| s.as_str()).sorted().collect_vec()
    }

    pub(crate) fn accumulated_votes(&self) -> AccumulatedVotes {
        let round = self.current_round();

        let accumulated_votes = round
            .iter()
            .into_group_map_by(|(_, &v)| v)
            .iter()
            .map(|(k, v)| (self.get_vote(*k), v.len()))
            .filter(|(_k, v)| *v > 0)
            .sorted_by(|a, b| Ord::cmp(&a.1, &b.1).then_with(|| Ord::cmp(&a.0, &b.0).reverse()))
            .rev()
            .map(|(v, c)| (v.to_owned(), c))
            .collect::<Vec<_>>();

        AccumulatedVotes {
            results: accumulated_votes,
        }
    }
}

pub(crate) struct AccumulatedVotes {
    pub results: Vec<(String, usize)>,
}

impl AccumulatedVotes {
    pub(crate) fn max_votes(&self) -> usize {
        self.results.iter().map(|(_k, v)| *v).max().unwrap_or(1)
    }

    pub(crate) fn all_with_max_votes(&self) -> Vec<String> {
        self.results
            .iter()
            .filter(|(_k, v)| *v == self.max_votes())
            .map(|(k, _v)| k.to_string())
            .collect::<Vec<_>>()
    }
}
