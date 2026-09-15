//! List-name resolution: exact match wins, then unique prefix, then unique
//! substring; case- and accent-insensitive. Ambiguity names the candidates.

/// Fold case and the common Latin diacritics. Dependency-free; covers what a
/// list name typed by a European user needs, not all of Unicode.
pub fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä'
            | 'Å' | 'Ā' | 'Ă' | 'Ą' => 'a',
            'ç' | 'ć' | 'č' | 'ĉ' | 'Ç' | 'Ć' | 'Č' | 'Ĉ' => 'c',
            'ď' | 'đ' | 'Ď' | 'Đ' => 'd',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' | 'ě' | 'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ę' | 'Ě' => {
                'e'
            }
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'Ì' | 'Í' | 'Î' | 'Ï' | 'Ī' => 'i',
            'ł' | 'ľ' | 'ĺ' | 'Ł' | 'Ľ' | 'Ĺ' => 'l',
            'ñ' | 'ń' | 'ň' | 'Ñ' | 'Ń' | 'Ň' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ő' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ő' => {
                'o'
            }
            'ř' | 'ŕ' | 'Ř' | 'Ŕ' => 'r',
            'ś' | 'š' | 'ş' | 'Ś' | 'Š' | 'Ş' => 's',
            'ť' | 'ţ' | 'Ť' | 'Ţ' => 't',
            'ù' | 'ú' | 'û' | 'ü' | 'ů' | 'ű' | 'ū' | 'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ů' | 'Ű' | 'Ū' => {
                'u'
            }
            'ý' | 'ÿ' | 'Ý' => 'y',
            'ź' | 'ž' | 'ż' | 'Ź' | 'Ž' | 'Ż' => 'z',
            'ß' => 's',
            other => other.to_ascii_lowercase(),
        })
        .collect::<String>()
        .to_lowercase()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<'a> {
    Found(&'a str),
    Ambiguous(Vec<&'a str>),
    NotFound,
}

/// `names` is `(id, display_name)`. Returns the matching id.
pub fn resolve_list<'a>(query: &str, names: &'a [(String, String)]) -> Resolution<'a> {
    let q = fold(query.trim());
    if q.is_empty() {
        return Resolution::NotFound;
    }
    let folded: Vec<(&str, String)> = names
        .iter()
        .map(|(id, name)| (id.as_str(), fold(name)))
        .collect();

    let exact: Vec<&str> = folded
        .iter()
        .filter(|(_, n)| *n == q)
        .map(|(id, _)| *id)
        .collect();
    match exact.len() {
        1 => return Resolution::Found(exact[0]),
        n if n > 1 => return Resolution::Ambiguous(names_for(&exact, names)),
        _ => {}
    }
    let prefix: Vec<&str> = folded
        .iter()
        .filter(|(_, n)| n.starts_with(&q))
        .map(|(id, _)| *id)
        .collect();
    match prefix.len() {
        1 => return Resolution::Found(prefix[0]),
        n if n > 1 => return Resolution::Ambiguous(names_for(&prefix, names)),
        _ => {}
    }
    let substring: Vec<&str> = folded
        .iter()
        .filter(|(_, n)| n.contains(&q))
        .map(|(id, _)| *id)
        .collect();
    match substring.len() {
        1 => Resolution::Found(substring[0]),
        0 => Resolution::NotFound,
        _ => Resolution::Ambiguous(names_for(&substring, names)),
    }
}

fn names_for<'a>(ids: &[&str], names: &'a [(String, String)]) -> Vec<&'a str> {
    names
        .iter()
        .filter(|(id, _)| ids.contains(&id.as_str()))
        .map(|(_, n)| n.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lists() -> Vec<(String, String)> {
        [
            ("1", "Tasks"),
            ("2", "Work"),
            ("3", "Work – Archive"),
            ("4", "Nákupy"),
            ("5", "Flagged email"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
    }

    #[test]
    fn exact_beats_prefix_beats_substring() {
        let l = lists();
        assert_eq!(resolve_list("work", &l), Resolution::Found("2"));
        assert_eq!(resolve_list("Work – Arch", &l), Resolution::Found("3"));
        assert_eq!(resolve_list("archive", &l), Resolution::Found("3"));
        assert_eq!(resolve_list("nakupy", &l), Resolution::Found("4"));
        assert_eq!(resolve_list("NÁKUPY", &l), Resolution::Found("4"));
        assert_eq!(resolve_list("nope", &l), Resolution::NotFound);
        assert_eq!(resolve_list("", &l), Resolution::NotFound);
    }

    #[test]
    fn ambiguity_names_candidates() {
        let l = lists();
        match resolve_list("wor", &l) {
            Resolution::Found(id) => assert_eq!(id, "2", "prefix 'wor' matches two, exact none"),
            Resolution::Ambiguous(c) => assert_eq!(c, vec!["Work", "Work – Archive"]),
            Resolution::NotFound => panic!(),
        }
    }
}
