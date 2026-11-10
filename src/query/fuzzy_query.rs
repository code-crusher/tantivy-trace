use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA};
use once_cell::sync::OnceCell;
use tantivy_fst::Automaton;

use crate::query::{AutomatonWeight, EnableScoring, Query, Weight};
use crate::schema::{Term, Type};
use crate::TantivyError::InvalidArgument;

/// State of the Levenshtein automaton a [`FuzzyTermQuery`] matches terms with.
///
/// Obtained from [`Automaton::start`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DfaState {
    dfa_state: u32,
    /// Whether a prefix of the term was already within the allowed distance.
    ///
    /// Always `false` for non-prefix queries.
    prefix_matched: bool,
}

/// The Levenshtein automaton a [`FuzzyTermQuery`] matches terms with.
///
/// Obtained from [`FuzzyTermQuery::automaton`], e.g. to stream the term dictionary
/// and collect the terms the query expands to.
pub struct DfaWrapper {
    dfa: DFA,
    prefix: bool,
}

impl DfaWrapper {
    pub(crate) fn new(dfa: DFA, prefix: bool) -> DfaWrapper {
        DfaWrapper { dfa, prefix }
    }

    fn is_exact(&self, dfa_state: u32) -> bool {
        matches!(self.dfa.distance(dfa_state), Distance::Exact(_))
    }
}

impl Automaton for DfaWrapper {
    type State = DfaState;

    fn start(&self) -> Self::State {
        let dfa_state = self.dfa.initial_state();
        DfaState {
            dfa_state,
            prefix_matched: self.prefix && self.is_exact(dfa_state),
        }
    }

    fn is_match(&self, state: &Self::State) -> bool {
        // For prefix queries, a match is sticky: once a prefix of the term was within the
        // allowed distance, the term matches whatever its remaining characters are.
        state.prefix_matched || self.is_exact(state.dfa_state)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        state.prefix_matched || state.dfa_state != levenshtein_automata::SINK_STATE
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        let dfa_state = self.dfa.transition(state.dfa_state, byte);
        DfaState {
            dfa_state,
            prefix_matched: state.prefix_matched || (self.prefix && self.is_exact(dfa_state)),
        }
    }
}

/// A Fuzzy Query matches all of the documents
/// containing a specific term that is within
/// Levenshtein distance
/// ```rust
/// use tantivy::collector::{Count, TopDocs};
/// use tantivy::query::FuzzyTermQuery;
/// use tantivy::schema::{Schema, TEXT};
/// use tantivy::{doc, Index, IndexWriter, Term};
///
/// fn example() -> tantivy::Result<()> {
///     let mut schema_builder = Schema::builder();
///     let title = schema_builder.add_text_field("title", TEXT);
///     let schema = schema_builder.build();
///     let index = Index::create_in_ram(schema);
///     {
///         let mut index_writer: IndexWriter = index.writer(15_000_000)?;
///         index_writer.add_document(doc!(
///             title => "The Name of the Wind",
///         ))?;
///         index_writer.add_document(doc!(
///             title => "The Diary of Muadib",
///         ))?;
///         index_writer.add_document(doc!(
///             title => "A Dairy Cow",
///         ))?;
///         index_writer.add_document(doc!(
///             title => "The Diary of a Young Girl",
///         ))?;
///         index_writer.commit()?;
///     }
///     let reader = index.reader()?;
///     let searcher = reader.searcher();
///
///     {
///         let term = Term::from_field_text(title, "Diary");
///         let query = FuzzyTermQuery::new(term, 1, true);
///         let (top_docs, count) = searcher.search(&query, &(TopDocs::with_limit(2).order_by_score(), Count)).unwrap();
///         assert_eq!(count, 2);
///         assert_eq!(top_docs.len(), 2);
///     }
///
///     Ok(())
/// }
/// # assert!(example().is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct FuzzyTermQuery {
    /// What term are we searching
    term: Term,
    /// How many changes are we going to allow
    distance: u8,
    /// Should a transposition cost 1 or 2?
    transposition_cost_one: bool,
    /// is a starts with query
    prefix: bool,
}

impl FuzzyTermQuery {
    /// Creates a new Fuzzy Query
    pub fn new(term: Term, distance: u8, transposition_cost_one: bool) -> FuzzyTermQuery {
        FuzzyTermQuery {
            term,
            distance,
            transposition_cost_one,
            prefix: false,
        }
    }

    /// Creates a new Fuzzy Query of the Term prefix
    pub fn new_prefix(term: Term, distance: u8, transposition_cost_one: bool) -> FuzzyTermQuery {
        FuzzyTermQuery {
            term,
            distance,
            transposition_cost_one,
            prefix: true,
        }
    }

    /// Returns the automaton this query matches terms with.
    ///
    /// For a JSON term, it matches the term's text only, not its JSON path.
    ///
    /// ```rust
    /// use tantivy::query::{DfaWrapper, FuzzyTermQuery};
    /// use tantivy::schema::{Schema, TEXT};
    /// use tantivy::Term;
    ///
    /// let mut schema_builder = Schema::builder();
    /// let title = schema_builder.add_text_field("title", TEXT);
    /// let query = FuzzyTermQuery::new(Term::from_field_text(title, "diary"), 1, true);
    /// let _automaton: DfaWrapper = query.automaton().unwrap();
    /// ```
    pub fn automaton(&self) -> crate::Result<DfaWrapper> {
        static AUTOMATON_BUILDER: [[OnceCell<LevenshteinAutomatonBuilder>; 2]; 3] = [
            [OnceCell::new(), OnceCell::new()],
            [OnceCell::new(), OnceCell::new()],
            [OnceCell::new(), OnceCell::new()],
        ];

        let automaton_builder = AUTOMATON_BUILDER
            .get(self.distance as usize)
            .ok_or_else(|| {
                InvalidArgument(format!(
                    "Levenshtein distance of {} is not allowed. Choose a value less than {}",
                    self.distance,
                    AUTOMATON_BUILDER.len()
                ))
            })?
            .get(self.transposition_cost_one as usize)
            .unwrap()
            .get_or_init(|| {
                LevenshteinAutomatonBuilder::new(self.distance, self.transposition_cost_one)
            });

        let term_value = self.term.value();

        let term_text = if term_value.typ() == Type::Json {
            if let Some(json_path_type) = term_value.json_path_type() {
                if json_path_type != Type::Str {
                    return Err(InvalidArgument(format!(
                        "The fuzzy term query requires a string path type for a json term. Found \
                         {json_path_type:?}"
                    )));
                }
            }

            std::str::from_utf8(self.term.serialized_value_bytes()).map_err(|_| {
                InvalidArgument(
                    "Failed to convert json term value bytes to utf8 string.".to_string(),
                )
            })?
        } else {
            term_value.as_str().ok_or_else(|| {
                InvalidArgument("The fuzzy term query requires a string term.".to_string())
            })?
        };
        // `build_prefix_dfa` loses matches whose best prefix is exactly at the maximum
        // distance when later characters push the full term above it. Instead, use the
        // plain DFA and keep track of whether a prefix already matched (see `DfaWrapper`).
        let automaton = automaton_builder.build_dfa(term_text);
        Ok(DfaWrapper::new(automaton, self.prefix))
    }

    fn specialized_weight(&self) -> crate::Result<AutomatonWeight<DfaWrapper>> {
        let automaton = self.automaton()?;
        if let Some((json_path_bytes, _)) = self.term.value().as_json() {
            Ok(AutomatonWeight::new_for_json_path(
                self.term.field(),
                automaton,
                json_path_bytes,
            ))
        } else {
            Ok(AutomatonWeight::new(self.term.field(), automaton))
        }
    }
}

impl Query for FuzzyTermQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        Ok(Box::new(self.specialized_weight()?))
    }
}

#[cfg(test)]
mod test {
    use super::FuzzyTermQuery;
    use crate::collector::{Count, TopDocs};
    use crate::indexer::NoMergePolicy;
    use crate::query::QueryParser;
    use crate::schema::{Schema, STORED, TEXT};
    use crate::{assert_nearly_equals, Index, IndexWriter, TantivyDocument, Term};

    #[test]
    pub fn test_fuzzy_prefix_does_not_miss_late_prefix_matches() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let name = schema_builder.add_text_field("name", TEXT);
        let index = Index::create_in_ram(schema_builder.build());
        let mut index_writer: IndexWriter = index.writer_for_tests()?;
        // "bd" (a prefix of "bdaa") is two edits away from "bacd": removing "a" and "c".
        // The best prefix is exactly at the maximum distance, and the last characters of
        // "bdaa" push the distance to the full term above it.
        index_writer.add_document(doc!(name => "bdaa"))?;
        // Same with transpositions disabled: "ö" is two edits away from "caö".
        index_writer.add_document(doc!(name => "ödd"))?;
        index_writer.commit()?;
        let searcher = index.reader()?.searcher();

        let query = FuzzyTermQuery::new_prefix(Term::from_field_text(name, "bacd"), 2, true);
        assert_eq!(searcher.search(&query, &Count)?, 1);
        let query = FuzzyTermQuery::new_prefix(Term::from_field_text(name, "bacd"), 2, false);
        assert_eq!(searcher.search(&query, &Count)?, 1);

        let query = FuzzyTermQuery::new_prefix(Term::from_field_text(name, "caö"), 2, false);
        assert_eq!(searcher.search(&query, &Count)?, 1);
        Ok(())
    }

    /// Optimal string alignment distance, with transpositions optionally costing one.
    fn osa_distance(left: &[char], right: &[char], transposition_cost_one: bool) -> usize {
        let mut dist = vec![vec![0usize; right.len() + 1]; left.len() + 1];
        for (i, row) in dist.iter_mut().enumerate() {
            row[0] = i;
        }
        for j in 0..=right.len() {
            dist[0][j] = j;
        }
        for i in 1..=left.len() {
            for j in 1..=right.len() {
                let substitution_cost = usize::from(left[i - 1] != right[j - 1]);
                dist[i][j] = (dist[i - 1][j] + 1)
                    .min(dist[i][j - 1] + 1)
                    .min(dist[i - 1][j - 1] + substitution_cost);
                if transposition_cost_one
                    && i > 1
                    && j > 1
                    && left[i - 1] == right[j - 2]
                    && left[i - 2] == right[j - 1]
                {
                    dist[i][j] = dist[i][j].min(dist[i - 2][j - 2] + 1);
                }
            }
        }
        dist[left.len()][right.len()]
    }

    fn prefix_distance(term: &str, query: &str, transposition_cost_one: bool) -> usize {
        let term_chars: Vec<char> = term.chars().collect();
        let query_chars: Vec<char> = query.chars().collect();
        (0..=term_chars.len())
            .map(|len| osa_distance(&term_chars[..len], &query_chars, transposition_cost_one))
            .min()
            .unwrap()
    }

    fn words(alphabet: &[char], len: usize) -> Vec<String> {
        if len == 0 {
            return vec![String::new()];
        }
        let mut result = Vec::new();
        for word in words(alphabet, len - 1) {
            for &c in alphabet {
                let mut word = word.clone();
                word.push(c);
                result.push(word);
            }
        }
        result
    }

    #[test]
    fn test_fuzzy_prefix_agrees_with_osa_oracle() -> crate::Result<()> {
        use tantivy_fst::Automaton;

        let alphabet = ['a', 'b', 'c', 'ö'];
        let terms = words(&alphabet, 3);
        let queries = words(&alphabet, 3);
        for &transposition_cost_one in &[false, true] {
            for &distance in &[1u8, 2u8] {
                for query in &queries {
                    let field = crate::schema::Field::from_field_id(0);
                    let query_term = Term::from_field_text(field, query);
                    let automaton =
                        FuzzyTermQuery::new_prefix(query_term, distance, transposition_cost_one)
                            .automaton()?;
                    for term in &terms {
                        let mut state = automaton.start();
                        for byte in term.as_bytes() {
                            state = automaton.accept(&state, *byte);
                        }
                        let expected = prefix_distance(term, query, transposition_cost_one)
                            <= distance as usize;
                        assert_eq!(
                            automaton.is_match(&state),
                            expected,
                            "query={query:?} term={term:?} distance={distance} \
                             transposition_cost_one={transposition_cost_one}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    pub fn test_fuzzy_automaton_streams_expanded_terms() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let title = schema_builder.add_text_field("title", TEXT);
        let index = Index::create_in_ram(schema_builder.build());
        let mut index_writer: IndexWriter = index.writer_for_tests()?;
        index_writer.add_document(doc!(title => "The Diary of a Dairy Cow"))?;
        index_writer.add_document(doc!(title => "A Daily Log"))?;
        index_writer.commit()?;
        let searcher = index.reader()?.searcher();

        let query = FuzzyTermQuery::new(Term::from_field_text(title, "diary"), 1, true);
        let automaton = query.automaton()?;
        let inverted_index = searcher.segment_reader(0).inverted_index(title)?;
        let mut stream = inverted_index.terms().search(automaton).into_stream()?;
        let mut terms = Vec::new();
        while stream.advance() {
            terms.push(String::from_utf8(stream.key().to_vec()).unwrap());
        }
        assert_eq!(terms, vec!["dairy", "diary"]);
        assert_eq!(searcher.search(&query, &Count)?, 1);
        Ok(())
    }

    #[test]
    pub fn test_fuzzy_json_path() -> crate::Result<()> {
        // # Defining the schema
        let mut schema_builder = Schema::builder();
        let attributes = schema_builder.add_json_field("attributes", TEXT | STORED);
        let schema = schema_builder.build();

        // # Indexing documents
        let index = Index::create_in_ram(schema.clone());

        let mut index_writer = index.writer_for_tests()?;
        index_writer.set_merge_policy(Box::new(NoMergePolicy));
        let doc = TantivyDocument::parse_json(
            &schema,
            r#"{
            "attributes": {
                "a": "japan"
            }
        }"#,
        )?;
        index_writer.add_document(doc)?;
        let doc = TantivyDocument::parse_json(
            &schema,
            r#"{
            "attributes": {
                "aa": "japan"
            }
        }"#,
        )?;
        index_writer.add_document(doc)?;
        index_writer.commit()?;

        let reader = index.reader()?;
        let searcher = reader.searcher();

        // # Fuzzy search
        let query_parser = QueryParser::for_index(&index, vec![attributes]);

        let get_json_path_term = |query: &str| -> crate::Result<Term> {
            let query = query_parser.parse_query(query)?;
            let mut terms = Vec::new();
            query.query_terms(&mut |term, _| {
                terms.push(term.clone());
            });

            Ok(terms[0].clone())
        };

        // shall not match the first document due to json path mismatch
        {
            let term = get_json_path_term("attributes.aa:japan")?;
            let fuzzy_query = FuzzyTermQuery::new(term, 2, true);
            let top_docs =
                searcher.search(&fuzzy_query, &TopDocs::with_limit(2).order_by_score())?;
            assert_eq!(top_docs.len(), 1, "Expected only 1 document");
            assert_eq!(top_docs[0].1.doc_id, 1, "Expected the second document");
        }

        // shall match the first document because Levenshtein distance is 1 (substitute 'o' with
        // 'a')
        {
            let term = get_json_path_term("attributes.a:japon")?;

            let fuzzy_query = FuzzyTermQuery::new(term, 1, true);
            let top_docs =
                searcher.search(&fuzzy_query, &TopDocs::with_limit(2).order_by_score())?;
            assert_eq!(top_docs.len(), 1, "Expected only 1 document");
            assert_eq!(top_docs[0].1.doc_id, 0, "Expected the first document");
        }

        // shall not match because non-prefix Levenshtein distance is more than 1 (add 'a' and 'n')
        {
            let term = get_json_path_term("attributes.a:jap")?;

            let fuzzy_query = FuzzyTermQuery::new(term, 1, true);
            let top_docs =
                searcher.search(&fuzzy_query, &TopDocs::with_limit(2).order_by_score())?;
            assert_eq!(top_docs.len(), 0, "Expected no document");
        }

        Ok(())
    }

    #[test]
    pub fn test_fuzzy_term() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let country_field = schema_builder.add_text_field("country", TEXT);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);
        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;
            index_writer.add_document(doc!(
                country_field => "japan",
            ))?;
            index_writer.add_document(doc!(
                country_field => "korea",
            ))?;
            index_writer.commit()?;
        }
        let reader = index.reader()?;
        let searcher = reader.searcher();

        // passes because Levenshtein distance is 1 (substitute 'o' with 'a')
        {
            let term = Term::from_field_text(country_field, "japon");
            let fuzzy_query = FuzzyTermQuery::new(term, 1, true);
            let top_docs =
                searcher.search(&fuzzy_query, &TopDocs::with_limit(2).order_by_score())?;
            assert_eq!(top_docs.len(), 1, "Expected only 1 document");
            let (score, _) = top_docs[0];
            assert_nearly_equals!(1.0, score);
        }

        // fails because non-prefix Levenshtein distance is more than 1 (add 'a' and 'n')
        {
            let term = Term::from_field_text(country_field, "jap");

            let fuzzy_query = FuzzyTermQuery::new(term, 1, true);
            let top_docs =
                searcher.search(&fuzzy_query, &TopDocs::with_limit(2).order_by_score())?;
            assert_eq!(top_docs.len(), 0, "Expected no document");
        }

        // passes because prefix Levenshtein distance is 0
        {
            let term = Term::from_field_text(country_field, "jap");
            let fuzzy_query = FuzzyTermQuery::new_prefix(term, 1, true);
            let top_docs =
                searcher.search(&fuzzy_query, &TopDocs::with_limit(2).order_by_score())?;
            assert_eq!(top_docs.len(), 1, "Expected only 1 document");
            let (score, _) = top_docs[0];
            assert_nearly_equals!(1.0, score);
        }
        Ok(())
    }

    #[test]
    pub fn test_fuzzy_term_transposition_cost_one() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let country_field = schema_builder.add_text_field("country", TEXT);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);
        let mut index_writer: IndexWriter = index.writer_for_tests()?;
        index_writer.add_document(doc!(country_field => "japan"))?;
        index_writer.commit()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let term_jaapn = Term::from_field_text(country_field, "jaapn");
        {
            let fuzzy_query_transposition = FuzzyTermQuery::new(term_jaapn.clone(), 1, true);
            let count = searcher.search(&fuzzy_query_transposition, &Count)?;
            assert_eq!(count, 1);
        }
        {
            let fuzzy_query_transposition = FuzzyTermQuery::new(term_jaapn, 1, false);
            let count = searcher.search(&fuzzy_query_transposition, &Count)?;
            assert_eq!(count, 0);
        }
        Ok(())
    }
}
