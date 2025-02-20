/// Score of base matching algorithm(fzy, skim, etc).
pub type Score = i32;

/// A tuple of (score, matched_indices) for the line has a match given the query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    pub score: Score,
    pub indices: Vec<usize>,
}

impl MatchResult {
    pub fn new(score: Score, indices: Vec<usize>) -> Self {
        Self { score, indices }
    }

    pub fn add_score(&mut self, score: Score) {
        self.score += score;
    }

    pub fn extend_indices(&mut self, indices: Vec<usize>) {
        self.indices.extend(indices);
        self.indices.sort_unstable();
        self.indices.dedup();
    }
}
