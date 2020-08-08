use crate::composition::{
    Atom, Composition, MatchAtom, Quantifier, RegexMatcher, StringMatcher, TrueAtom,
};
use crate::rule;
use crate::tokenizer::Token;
use crate::{structure, utils, Error};
use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use std::convert::TryFrom;

fn atoms_from_token(
    token: &structure::Token,
    case_sensitive: bool,
) -> Vec<(Box<dyn Atom>, Quantifier)> {
    let mut atoms = Vec::new();

    let case_sensitive = match &token.case_sensitive {
        Some(string) => string == "yes",
        None => case_sensitive,
    };
    let min = token
        .min
        .clone()
        .map(|x| x.parse().expect("can't parse min as usize"))
        .unwrap_or(1usize);
    let max = token
        .max
        .clone()
        .map(|x| x.parse().expect("can't parse max as usize"))
        .unwrap_or(1usize);

    let quantifier = Quantifier::new(min, max);

    let is_regex = token.regexp.clone().map_or(false, |x| x == "yes");
    let accessor: Box<dyn for<'a> Fn(&'a Token) -> &'a str> = if case_sensitive {
        Box::new(|token: &Token| token.text)
    } else {
        Box::new(|token: &Token| token.lower.as_str())
    };

    if is_regex {
        let regex = utils::fix_regex(&token.text);
        let regex = RegexBuilder::new(&regex)
            .case_insensitive(!case_sensitive)
            .build()
            .expect("invalid regex");
        let matcher = RegexMatcher::new(regex);
        atoms.push((
            Box::new(MatchAtom::new(matcher, accessor)) as Box<dyn Atom>,
            quantifier,
        ));
    } else {
        let text = if case_sensitive {
            token.text.to_string()
        } else {
            token.text.to_lowercase()
        };

        atoms.push((
            Box::new(MatchAtom::new(StringMatcher::new(text), accessor)) as Box<dyn Atom>,
            quantifier,
        ));
    };

    if let Some(to_skip) = token.skip.clone() {
        let to_skip = if to_skip == "-1" {
            20 // TODO: should be an option in config OR restricted to one sentence
        } else {
            to_skip.parse().expect("can't parse skip as usize or -1")
        };
        atoms.push((Box::new(TrueAtom::new()), Quantifier::new(0, to_skip)));
    }

    atoms
}

impl From<Vec<structure::SuggestionPart>> for rule::Suggester {
    fn from(data: Vec<structure::SuggestionPart>) -> rule::Suggester {
        let mut parts = Vec::new();

        lazy_static! {
            static ref MATCH_REGEX: Regex = Regex::new(r"\\(\d)").unwrap();
        }

        for part in data {
            match part {
                structure::SuggestionPart::Text(text) => {
                    let mut end_index = 0;

                    for capture in MATCH_REGEX.captures_iter(&text) {
                        let mat = capture.get(0).unwrap();
                        if end_index != mat.start() {
                            parts.push(rule::SuggesterPart::Text(
                                (&text[end_index..mat.start()]).to_string(),
                            ))
                        }

                        let index = capture
                            .get(1)
                            .unwrap()
                            .as_str()
                            .parse::<usize>()
                            .expect("match regex capture must be parsable as usize.")
                            - 1;

                        parts.push(rule::SuggesterPart::Match(rule::Match::new(
                            index,
                            Box::new(|x| x.to_string()),
                        )));
                        end_index = mat.end();
                    }

                    if end_index < text.len() {
                        parts.push(rule::SuggesterPart::Text((&text[end_index..]).to_string()))
                    }
                }
                structure::SuggestionPart::Match(m) => {
                    let index =
                        m.no.parse::<usize>()
                            .expect("no must be parsable as usize.")
                            - 1;

                    let case_conversion = if let Some(conversion) = &m.case_conversion {
                        Some(conversion.as_str())
                    } else {
                        None
                    };

                    parts.push(rule::SuggesterPart::Match(rule::Match::new(
                        index,
                        match case_conversion {
                            Some("alllower") => Box::new(|x| x.to_lowercase()),
                            Some("startlower") => Box::new(|x| {
                                utils::apply_to_first(x, |c| c.to_lowercase().collect())
                            }),
                            Some("startupper") => Box::new(|x| {
                                utils::apply_to_first(x, |c| c.to_uppercase().collect())
                            }),
                            Some(x) => panic!("case conversion {} not supported.", x),
                            None => Box::new(|x| x.to_string()),
                        },
                    )));
                }
            }
        }

        rule::Suggester { parts }
    }
}

impl TryFrom<structure::Rule> for rule::Rule {
    type Error = Error;

    fn try_from(data: structure::Rule) -> Result<rule::Rule, Self::Error> {
        let mut start = None;
        let mut end = None;

        let mut atoms = Vec::new();
        let case_sensitive = match &data.pattern.case_sensitive {
            Some(string) => string == "yes",
            None => false,
        };

        for part in &data.pattern.parts {
            match part {
                structure::PatternPart::Token(token) => {
                    atoms.extend(atoms_from_token(token, case_sensitive))
                }
                structure::PatternPart::Marker(marker) => {
                    start = Some(atoms.len());

                    let mut last_length = 0;

                    for token in &marker.tokens {
                        let atoms_to_add = atoms_from_token(token, case_sensitive);
                        last_length = atoms_to_add.len();
                        atoms.extend(atoms_to_add);
                    }

                    // if the last token needs more than one atom, only include the first one (needed for e. g. when using "skip")
                    end = Some(atoms.len() - (std::cmp::max(last_length - 1, 0)));
                }
            }
        }

        let start = start.unwrap_or(0);
        let end = end.unwrap_or_else(|| atoms.len());

        let suggesters = data
            .message
            .parts
            .into_iter()
            .filter_map(|x| match x {
                structure::MessagePart::Suggestion(suggestion) => Some(suggestion.parts.into()),
                structure::MessagePart::Text(_) => None,
            })
            .collect::<Vec<rule::Suggester>>();

        if suggesters.is_empty() {
            return Err(Error::Unimplemented("rule with no suggestion".into()));
        }

        let mut tests = Vec::new();
        for example in &data.examples {
            let mut texts = Vec::new();
            let mut char_length = 0;
            let mut suggestion: Option<rule::Suggestion> = None;

            for part in &example.parts {
                match part {
                    structure::ExamplePart::Text(text) => {
                        texts.push(text.as_str());
                        char_length += text.chars().count();
                    }
                    structure::ExamplePart::Marker(marker) => {
                        if suggestion.is_some() {
                            return Err(Error::Unexpected(
                                "example must have one or zero markers".into(),
                            ));
                        }

                        texts.push(marker.text.as_str());
                        let length = marker.text.chars().count();

                        if let Some(correction_text) = &example.correction {
                            suggestion = Some(rule::Suggestion {
                                start: char_length,
                                end: char_length + length,
                                text: correction_text.split('|').map(|x| x.to_string()).collect(),
                            });
                        }

                        char_length += marker.text.chars().count();
                    }
                }
            }

            tests.push(rule::Test {
                text: texts.join(""),
                suggestion,
            });
        }

        let composition = Composition::new(atoms);

        Ok(rule::Rule {
            composition,
            tests,
            suggesters,
            start,
            end,
            id: String::new(),
        })
    }
}
