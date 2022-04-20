use std::collections::HashMap;

use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::*;
use tantivy::Score;

use super::DocFields;
use shared::config::Lens;

type QueryVec = Vec<(Occur, Box<dyn Query>)>;

fn _boosted_term(field: Field, term: &str, boost: Score) -> Box<BoostQuery> {
    Box::new(BoostQuery::new(
        Box::new(TermQuery::new(
            Term::from_field_text(field, term),
            // Needs WithFreqs otherwise scoring is wonky.
            IndexRecordOption::WithFreqs,
        )),
        boost,
    ))
}

pub fn build_query(
    fields: DocFields,
    lenses: &HashMap<String, Lens>,
    applied_lens: &[String],
    query_string: &str,
) -> BooleanQuery {
    // Tokenize query string
    let terms: Vec<&str> = query_string
        .split(' ')
        .into_iter()
        .map(|token| token.trim())
        .collect();

    log::info!("lenses: {:?}, terms: {:?}", applied_lens, terms);

    let mut lense_queries: QueryVec = Vec::new();
    for lens in applied_lens {
        if lenses.contains_key(lens) {
            let lens = lenses.get(lens).unwrap();
            for domain in &lens.domains {
                lense_queries.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(fields.domain, domain),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
        }
    }

    let mut term_query: QueryVec = Vec::new();
    for term in terms {
        // Emphasize matches in the content more than words in the title
        term_query.push((Occur::Should, _boosted_term(fields.content, term, 5.0)));
        term_query.push((Occur::Should, _boosted_term(fields.title, term, 0.25)));
    }

    let mut nested_query: QueryVec = vec![(Occur::Must, Box::new(BooleanQuery::new(term_query)))];
    if !lense_queries.is_empty() {
        nested_query.push((Occur::Must, Box::new(BooleanQuery::new(lense_queries))));
    }

    BooleanQuery::new(nested_query)
}