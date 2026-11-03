//! Workloads: the kinds of model work hatch does, each routable to its own model chain.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Workload {
    Main,
    Triage,
    Draft,
    Compaction,
    MemoryFlush,
    /// Scheduled daily brief generation.
    Brief,
    /// Scheduled feed generation (same facts as the brief, different voice).
    Feed,
}

impl Workload {
    pub const ALL: [Workload; 7] =
        [Workload::Main, Workload::Triage, Workload::Draft, Workload::Compaction, Workload::MemoryFlush, Workload::Brief, Workload::Feed];

    pub fn as_str(&self) -> &'static str {
        match self {
            Workload::Main => "main",
            Workload::Triage => "triage",
            Workload::Draft => "draft",
            Workload::Compaction => "compaction",
            Workload::MemoryFlush => "memory_flush",
            Workload::Brief => "brief",
            Workload::Feed => "feed",
        }
    }

    pub fn parse(s: &str) -> Option<Workload> {
        Self::ALL.into_iter().find(|w| w.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for w in Workload::ALL {
            assert_eq!(Workload::parse(w.as_str()), Some(w));
        }
        assert_eq!(Workload::parse("nope"), None);
    }
}
