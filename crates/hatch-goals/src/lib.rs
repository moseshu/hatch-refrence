//! `hatch-goals` — Goals, Ideas and Feed: the product surface over
//! background-worker output.
//!
//! Why a separate crate instead of extending `hatch-workers`: workers are
//! scheduled jobs; goals/ideas/feed are user data with their own storage
//! and CLI surface. Goals live on disk
//! (`$HATCH_HOME/goals/<id>/GOAL.md`, `files/`, `hidden_files/`);
//! ideas and feed items live in SQLite (same convention as claims,
//! briefs and worker outputs). The feed worker itself lives here too
//! because it is the feed's producer, and it reuses the brief's fact
//! gathering so the two never disagree about what "recent facts" are.
//!
//! Everything in this crate that resembles a Muse-framework behaviour
//! and was not observed in Muse evidence is marked ORIGINAL design in
//! its module docs — the goal status machine, the attention levels and
//! the feed intro rule are this codebase's own choices.

pub mod feed;
pub mod goals;
pub mod ideas;

pub use feed::{
    goal_attention, FeedItem, FeedStore, FeedWorker, DEFAULT_PROMPT, INTRO_TEXT, KIND as FEED_KIND,
};
pub use goals::{attention_level, Attention, Goal, GoalStatus, GoalStore};
pub use ideas::{Idea, IdeaStatus, IdeaStore, WORKER_SUGGESTION_CONFIDENCE};
