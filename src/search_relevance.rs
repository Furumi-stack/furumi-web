/// Common relevance scale for ordinary local and federated catalogue hits.
pub(crate) struct SearchRelevance {
    phrase: String,
    terms: Vec<String>,
}

fn normalize(text: &str) -> String {
    music_dht::normalize_name(text).replace('ё', "е")
}

impl SearchRelevance {
    pub(super) fn new(query: &str) -> Self {
        const STOP: &str = "а в во для за и из к на о об от по под с со у a an and at by for from in of on or the to with";
        let phrase = normalize(query);
        let mut terms = Vec::new();
        for word in phrase.split_whitespace() {
            if !STOP.split_whitespace().any(|stop| stop == word)
                && !terms.iter().any(|term| term == word)
            {
                terms.push(word.to_owned());
            }
        }
        Self { phrase, terms }
    }

    pub(super) fn score<'a>(
        &self,
        title: &str,
        credits: impl Iterator<Item = &'a str>,
        release: Option<&str>,
    ) -> Option<u32> {
        if self.phrase.is_empty() {
            return None;
        }
        let title = normalize(title);
        if title == self.phrase {
            return Some(1000);
        }
        let fields: Vec<_> = std::iter::once(title)
            .chain(credits.map(normalize))
            .chain(release.map(normalize))
            .collect();
        for (i, field) in fields.iter().enumerate() {
            // Token boundaries keep short queries such as "из" out of "жизнь".
            if field.match_indices(&self.phrase).any(|(start, matched)| {
                (start == 0 || field[..start].ends_with(' '))
                    && (start + matched.len() == field.len()
                        || field[start + matched.len()..].starts_with(' '))
            }) {
                return Some(if i == 0 { 950 } else { 800 });
            }
        }
        if self.terms.is_empty() {
            return None;
        }
        let mut matched = 0;
        let mut exact = 0;
        let mut title_hits = 0;
        for term in &self.terms {
            let allow_prefix = term.chars().nth(1).is_some();
            let mut found = false;
            let mut full_word = false;
            let mut in_title = false;
            for (i, field) in fields.iter().enumerate() {
                for word in field.split_whitespace() {
                    if word == term || (allow_prefix && word.starts_with(term)) {
                        found = true;
                        full_word |= word == term;
                        in_title |= i == 0;
                    }
                }
            }
            matched += u32::from(found);
            exact += u32::from(full_word);
            title_hits += u32::from(in_title);
        }
        let total = self.terms.len() as u32;
        // Permit partial titles, but do not display a long query's single hit.
        (3 * matched >= 2 * total)
            .then_some((500 * matched + 160 * exact + 130 * title_hits) / total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_search_filters_incidental_words_and_orders_title_evidence() {
        let query = SearchRelevance::new("Девушки из высшего общества");
        let score = |s: &str| query.score(s, std::iter::empty(), None);
        assert_eq!(score("Рэп из-под стола (Remastered)"), None);
        assert_eq!(score("Девушки"), None);
        assert!(score("Из высшего общества").is_some());
        assert!(score("Девушки из высшего общества") > score("Девушки из высшего общества (Live)"));
        assert!(score("Девушки из высшего общества (Live)") > score("Из высшего общества"));
    }

    #[test]
    fn mixed_title_artist_queries_search_credits_and_albums() {
        let query = SearchRelevance::new("Саша Скул Вино из цикуты");
        assert!(
            query
                .score("Вино из цикуты", ["Саша Скул"].into_iter(), None)
                .is_some()
        );
        assert!(
            query
                .score("Интро", ["Саша Скул"].into_iter(), Some("Вино из цикуты"))
                .is_some()
        );
        assert!(
            query
                .score("Интро", ["Саша Скул"].into_iter(), None)
                .is_none()
        );
        let query = SearchRelevance::new("The The");
        assert!(query.score("The The", std::iter::empty(), None).is_some());
        assert!(query.score("There", std::iter::empty(), None).is_none());
    }
}
