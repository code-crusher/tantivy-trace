use query_grammar::Occur;

use crate::query::Weight;

/// A replacement for `HashMap<Occur, Vec<Box<dyn Weight>>>`.
///
/// `Occur` only has 3 possible variants, so we store one `Vec<Box<dyn Weight>>` per variant
/// instead of paying for hashing.
#[derive(Default)]
pub struct OccurWeights {
    must: Vec<Box<dyn Weight>>,
    should: Vec<Box<dyn Weight>>,
    must_not: Vec<Box<dyn Weight>>,
}

impl OccurWeights {
    pub fn push(&mut self, occur: Occur, weight: Box<dyn Weight>) {
        let occur_weights = match occur {
            Occur::Must => &mut self.must,
            Occur::Should => &mut self.should,
            Occur::MustNot => &mut self.must_not,
        };
        occur_weights.push(weight);
    }

    pub fn get(&self, occur: Occur) -> &[Box<dyn Weight>] {
        match occur {
            Occur::Must => &self.must,
            Occur::Should => &self.should,
            Occur::MustNot => &self.must_not,
        }
    }

    /// Moves all SHOULD weights into MUST, emptying the SHOULD bucket.
    ///
    /// Used when the number of SHOULD clauses equals the minimum number of should clauses
    /// required to match: at that point every SHOULD clause is effectively required.
    pub fn promote_should_to_must(&mut self) {
        let should_weights = std::mem::take(&mut self.should);
        self.must.extend(should_weights);
    }

    /// Stable-sorts the MUST weights by [`Weight::refutation_cost`].
    ///
    /// Building an intersection stops at the first empty scorer, so trying the cheapest
    /// clauses first lets a selective clause refute the intersection before an expensive
    /// scorer gets built. The sort is stable, so clauses with the same cost keep their
    /// query order.
    pub fn sort_must_by_refutation_cost(&mut self) {
        self.must.sort_by_key(|weight| weight.refutation_cost());
    }

    pub fn len(&self) -> usize {
        self.must.len() + self.should.len() + self.must_not.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (Occur, &Box<dyn Weight>)> {
        [Occur::Must, Occur::Should, Occur::MustNot]
            .into_iter()
            .flat_map(move |occur| self.get(occur).iter().map(move |weight| (occur, weight)))
    }
}

#[cfg(test)]
mod tests {
    use super::OccurWeights;
    use crate::query::{AllQuery, EnableScoring, Occur, Query, RegexQuery, TermQuery, Weight};
    use crate::schema::{IndexRecordOption, Schema, TEXT};
    use crate::{Index, Term};

    fn term_weight(searcher: &crate::Searcher, term: &str) -> Box<dyn Weight> {
        let field = searcher.schema().get_field("text").unwrap();
        TermQuery::new(Term::from_field_text(field, term), IndexRecordOption::Basic)
            .weight(EnableScoring::disabled_from_searcher(searcher))
            .unwrap()
    }

    fn test_searcher() -> crate::Searcher {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema_builder.build());
        index.reader().unwrap().searcher()
    }

    #[test]
    fn test_push_and_get() {
        let searcher = test_searcher();
        let mut weights = OccurWeights::default();
        weights.push(Occur::Must, term_weight(&searcher, "a"));
        weights.push(Occur::Must, term_weight(&searcher, "b"));
        weights.push(Occur::Should, term_weight(&searcher, "c"));
        assert_eq!(weights.get(Occur::Must).len(), 2);
        assert_eq!(weights.get(Occur::Should).len(), 1);
        assert!(weights.get(Occur::MustNot).is_empty());
        assert_eq!(weights.len(), 3);
    }

    #[test]
    fn test_promote_should_to_must() {
        let searcher = test_searcher();
        let mut weights = OccurWeights::default();
        weights.push(Occur::Must, term_weight(&searcher, "a"));
        weights.push(Occur::Should, term_weight(&searcher, "b"));
        weights.push(Occur::Should, term_weight(&searcher, "c"));
        weights.promote_should_to_must();
        assert_eq!(weights.get(Occur::Must).len(), 3);
        assert!(weights.get(Occur::Should).is_empty());
    }

    #[test]
    fn test_iter_visits_all_buckets_in_order() {
        let searcher = test_searcher();
        let mut weights = OccurWeights::default();
        weights.push(Occur::Must, term_weight(&searcher, "a"));
        weights.push(Occur::Should, term_weight(&searcher, "b"));
        weights.push(Occur::MustNot, term_weight(&searcher, "c"));
        let occurs: Vec<Occur> = weights.iter().map(|(occur, _)| occur).collect();
        assert_eq!(occurs, vec![Occur::Must, Occur::Should, Occur::MustNot]);
    }

    #[test]
    fn test_sort_must_by_refutation_cost() {
        let searcher = test_searcher();
        let field = searcher.schema().get_field("text").unwrap();
        let mut weights = OccurWeights::default();
        // The regex walks the term dictionary, so it is the most expensive to refute.
        weights.push(
            Occur::Must,
            RegexQuery::from_pattern(".*", field)
                .unwrap()
                .weight(EnableScoring::disabled_from_searcher(&searcher))
                .unwrap(),
        );
        weights.push(Occur::Must, term_weight(&searcher, "a"));
        // An all query can never refute anything, but building it is free.
        weights.push(
            Occur::Must,
            AllQuery
                .weight(EnableScoring::disabled_from_searcher(&searcher))
                .unwrap(),
        );
        weights.push(Occur::Should, term_weight(&searcher, "b"));

        weights.sort_must_by_refutation_cost();

        let costs: Vec<u64> = weights
            .get(Occur::Must)
            .iter()
            .map(|weight| weight.refutation_cost())
            .collect();
        assert_eq!(costs, vec![0, 1, 1_000]);
        // Sorting MUST does not touch the other buckets.
        assert_eq!(weights.get(Occur::Should).len(), 1);
        assert!(weights.get(Occur::MustNot).is_empty());
    }
}
